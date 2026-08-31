use crate::client::{
    admit_interactive_rpc_now, ClientOutboundAdmissionError, RpcConsumerKind, RpcGenerationScope,
};
use crate::domain::{lock_or_recover, ClientInner};
use crate::pane::mousestate::MouseState;
use crate::pane::renderable::{
    hydrate_lines, hydrate_render_application_lines, RenderableInner, RenderablePaneBinding,
    RenderableState,
};
use anyhow::{bail, Context};
use async_trait::async_trait;
use codec::*;
use config::configuration;
use config::keyassignment::ScrollbackEraseMode;
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern,
    SearchResult, WithPaneLines,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::{MuxSessionIncarnation, PaneRegistrationHandle, PaneRegistrationSlot};
use parking_lot::{Condvar, MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::TryFrom;
use std::future::Future;
use std::num::NonZeroU64;
use std::ops::Range;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use termwiz::input::KeyEvent;
use termwiz::surface::SequenceNo;
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, Progress, SemanticZone,
    StableRowIndex, TerminalConfiguration, TerminalSize,
};

const MAX_RENDER_APPLICATION_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Admit latency-critical input into the exact mux transport generation during
/// the input callback itself.  Awaiting the reply remains asynchronous, so a
/// slow or disconnected peer can never park the GUI thread.
fn dispatch_interactive_rpc<F, T>(request: F, operation: &'static str) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<T>> + 'static,
    T: 'static,
{
    let reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Input,
        4 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => anyhow::bail!(
            "main-thread scheduler rejected interactive mux RPC {operation} before transport admission: {rejected:?}"
        ),
    };
    let Some(request) = admit_interactive_rpc_now(request)? else {
        return Ok(());
    };
    reservation
        .spawn_local(async move {
            if let Err(error) = request.await {
                metrics::counter!(
                    "mux.client.interactive_rpc.detached_error.total",
                    "operation" => operation,
                )
                .increment(1);
                log::debug!("detached interactive mux RPC {operation} failed: {error:#}");
            }
        })
        .detach();
    Ok(())
}

const RELIABLE_INPUT_QUEUE_CAPACITY: usize = 4_096;
const RELIABLE_INPUT_QUEUE_BYTE_CAPACITY: usize = 1024 * 1024;
const RELIABLE_KEY_INPUT_ESTIMATED_BYTES: usize = 256;
const RELIABLE_PANE_WRITE_ENTRY_OVERHEAD_BYTES: usize = 256;
const RELIABLE_INPUT_TRANSPORT_RETRY_DELAY: Duration = Duration::from_millis(5);
const RELIABLE_INPUT_MAX_RETRY_DELAY: Duration = Duration::from_millis(100);
const RELIABLE_PANE_WRITE_NOTIFICATION_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReliableInputCodecDisposition {
    AwaitingAuthority,
    Legacy,
    Reliable,
    ReliableTraced,
}

fn reliable_input_codec_disposition(
    agreed_codec_version: Option<usize>,
) -> ReliableInputCodecDisposition {
    match agreed_codec_version {
        None => ReliableInputCodecDisposition::AwaitingAuthority,
        Some(version) if version < RELIABLE_KEY_EVENT_V1_MIN_CODEC_VERSION => {
            ReliableInputCodecDisposition::Legacy
        }
        Some(version) if version < RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION => {
            ReliableInputCodecDisposition::Reliable
        }
        Some(_) => ReliableInputCodecDisposition::ReliableTraced,
    }
}

fn reliable_input_retry_delay(base: Duration, consecutive_retries: u32) -> Duration {
    let multiplier = 1u32.checked_shl(consecutive_retries).unwrap_or(u32::MAX);
    // Never shorten an authoritative peer delay. The local ceiling only
    // bounds our exponential amplification of a smaller transport/scheduler
    // prompt.
    let ceiling = RELIABLE_INPUT_MAX_RETRY_DELAY.max(base);
    base.saturating_mul(multiplier).min(ceiling)
}

async fn yield_reliable_input_worker_once() {
    let mut yielded = false;
    futures::future::poll_fn(move |context| {
        if std::mem::replace(&mut yielded, true) {
            Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[derive(Clone)]
struct QueuedReliableInput {
    registration: PaneRegistrationHandle,
    pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
    payload: QueuedReliableInputPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReliablePaneInputAuthority {
    session_incarnation: MuxSessionIncarnation,
    pane_registration: ReliablePaneRegistrationIdentityV1,
}

/// Exact authority that fences one queued non-idempotent input attempt.
///
/// Reliable peers expose a durable server-session incarnation.  Codec-46
/// peers do not, so their non-idempotent legacy input PDUs are instead bound to
/// one physical client transport generation.  Keeping those authorities in a
/// closed enum prevents a synthetic connection identifier from being confused
/// with a server-issued session identity after an upgrade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReliableInputAttemptAuthority {
    ServerSession(MuxSessionIncarnation),
    LegacyTransport(NonZeroU64),
}

#[derive(Clone)]
enum QueuedReliableInputPayload {
    Key {
        request: ReliableKeyEventV1,
        trace_context: Option<SampledTraceContextV1>,
        initial_rpc_scope: Option<RpcGenerationScope>,
        attempted_authority: Option<ReliableInputAttemptAuthority>,
        effect_may_have_reached: bool,
    },
    PaneWrite {
        request: ReliablePaneWriteV1,
        reserved_serial_end: InputSerial,
        initial_rpc_scope: Option<RpcGenerationScope>,
        attempted_authority: Option<ReliableInputAttemptAuthority>,
        effect_may_have_reached: bool,
        delivery: Arc<ReliablePaneWriteDelivery>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReliablePaneWriteFailure {
    UnsupportedCodec,
    QueueFull,
    SchedulerRejected,
    PaneRetired,
    DomainDetached,
    ServerRestartAfterAmbiguousAttempt,
    Indeterminate,
    DefinitelyNotApplied,
    ProtocolError,
}

impl ReliablePaneWriteFailure {
    fn label(self) -> &'static str {
        match self {
            Self::UnsupportedCodec => "unsupported_codec",
            Self::QueueFull => "queue_full",
            Self::SchedulerRejected => "scheduler_rejected",
            Self::PaneRetired => "pane_retired",
            Self::DomainDetached => "domain_detached",
            Self::ServerRestartAfterAmbiguousAttempt => "server_restart_after_ambiguous_attempt",
            Self::Indeterminate => "indeterminate",
            Self::DefinitelyNotApplied => "definitely_not_applied",
            Self::ProtocolError => "protocol_error",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::UnsupportedCodec => {
                "The connected mux server must be upgraded before it can accept reliable pane input."
            }
            Self::QueueFull => "The bounded pane-input queue is full; input was not accepted.",
            Self::SchedulerRejected => {
                "The input scheduler rejected pane input before queue ownership transfer."
            }
            Self::PaneRetired => "The target pane retired before accepted input was delivered.",
            Self::DomainDetached => {
                "The remote domain detached before accepted input was delivered."
            }
            Self::ServerRestartAfterAmbiguousAttempt => {
                "The mux server restarted after an ambiguous write; replay was quarantined to prevent duplicate input."
            }
            Self::Indeterminate => {
                "The mux could not determine the exact applied input prefix; replay was quarantined."
            }
            Self::DefinitelyNotApplied => {
                "The remote pane writer rejected accepted input without applying bytes."
            }
            Self::ProtocolError => {
                "The reliable pane-input protocol returned an invalid terminal result."
            }
        }
    }

    fn io_kind(self) -> std::io::ErrorKind {
        match self {
            Self::QueueFull | Self::SchedulerRejected => std::io::ErrorKind::WouldBlock,
            Self::UnsupportedCodec => std::io::ErrorKind::Unsupported,
            Self::PaneRetired | Self::DomainDetached => std::io::ErrorKind::NotConnected,
            Self::ServerRestartAfterAmbiguousAttempt
            | Self::Indeterminate
            | Self::DefinitelyNotApplied
            | Self::ProtocolError => std::io::ErrorKind::Other,
        }
    }
}

#[derive(Default)]
struct ReliablePaneWriteDeliveryState {
    pending_chunks: usize,
    sticky_failure: Option<ReliablePaneWriteFailure>,
    last_notification: Option<Instant>,
}

struct ReliablePaneWriteDelivery {
    state: Mutex<ReliablePaneWriteDeliveryState>,
    changed: Condvar,
}

impl ReliablePaneWriteDelivery {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReliablePaneWriteDeliveryState::default()),
            changed: Condvar::new(),
        })
    }

    fn try_accept_chunk(&self) -> Result<(), ReliablePaneWriteFailure> {
        let mut state = self.state.lock();
        if let Some(failure) = state.sticky_failure {
            return Err(failure);
        }
        // Admission is already bounded to RELIABLE_INPUT_QUEUE_CAPACITY live
        // entries, so this counter cannot approach usize exhaustion.
        state.pending_chunks += 1;
        Ok(())
    }

    fn finish_chunk(&self, failure: Option<ReliablePaneWriteFailure>) {
        let mut state = self.state.lock();
        if state.pending_chunks == 0 {
            log::error!("reliable pane-write delivery settled a chunk without accepted ownership");
            state
                .sticky_failure
                .get_or_insert(ReliablePaneWriteFailure::ProtocolError);
        } else {
            state.pending_chunks -= 1;
        }
        if let Some(failure) = failure {
            state.sticky_failure.get_or_insert(failure);
        }
        self.changed.notify_all();
    }

    fn sticky_failure(&self) -> Option<ReliablePaneWriteFailure> {
        self.state.lock().sticky_failure
    }

    #[cfg(test)]
    fn pending_chunks(&self) -> usize {
        self.state.lock().pending_chunks
    }

    /// Atomically distinguish clean settlement, pending ownership, and a
    /// sticky terminal result. Reading failure and pending state separately
    /// could otherwise return a false successful flush when the worker settles
    /// a failed chunk between the two observations.
    fn flush_pending(&self) -> Result<bool, ReliablePaneWriteFailure> {
        let state = self.state.lock();
        if let Some(failure) = state.sticky_failure {
            Err(failure)
        } else {
            Ok(state.pending_chunks != 0)
        }
    }

    fn wait_until_settled(&self) -> Option<ReliablePaneWriteFailure> {
        let mut state = self.state.lock();
        while state.pending_chunks != 0 && state.sticky_failure.is_none() {
            self.changed.wait(&mut state);
        }
        state.sticky_failure
    }

    fn should_notify(&self) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock();
        if state.last_notification.is_some_and(|last| {
            now.saturating_duration_since(last) < RELIABLE_PANE_WRITE_NOTIFICATION_INTERVAL
        }) {
            return false;
        }
        state.last_notification = Some(now);
        true
    }
}

impl QueuedReliableInput {
    fn input_serial(&self) -> InputSerial {
        match &self.payload {
            QueuedReliableInputPayload::Key { request, .. } => request.input_serial,
            QueuedReliableInputPayload::PaneWrite { request, .. } => request.input_serial,
        }
    }

    #[allow(dead_code)]
    fn pane_id(&self) -> PaneId {
        match &self.payload {
            QueuedReliableInputPayload::Key { request, .. } => request.pane_id,
            QueuedReliableInputPayload::PaneWrite { request, .. } => request.pane_id,
        }
    }

    fn estimated_bytes(&self) -> usize {
        match &self.payload {
            QueuedReliableInputPayload::Key { .. } => RELIABLE_KEY_INPUT_ESTIMATED_BYTES,
            QueuedReliableInputPayload::PaneWrite { request, .. } => {
                RELIABLE_PANE_WRITE_ENTRY_OVERHEAD_BYTES.saturating_add(request.data.len())
            }
        }
    }

    fn is_pane_write(&self) -> bool {
        matches!(&self.payload, QueuedReliableInputPayload::PaneWrite { .. })
    }

    fn same_identity(&self, other: &Self) -> bool {
        if !self.registration.same_registration(&other.registration)
            || !Arc::ptr_eq(&self.pane_authority, &other.pane_authority)
        {
            return false;
        }
        match (&self.payload, &other.payload) {
            (
                QueuedReliableInputPayload::Key { request: left, .. },
                QueuedReliableInputPayload::Key { request: right, .. },
            ) => left == right,
            (
                QueuedReliableInputPayload::PaneWrite {
                    request: left,
                    reserved_serial_end: left_end,
                    delivery: left_delivery,
                    ..
                },
                QueuedReliableInputPayload::PaneWrite {
                    request: right,
                    reserved_serial_end: right_end,
                    delivery: right_delivery,
                    ..
                },
            ) => {
                left == right && left_end == right_end && Arc::ptr_eq(left_delivery, right_delivery)
            }
            _ => false,
        }
    }

    fn write_delivery(&self) -> Option<&Arc<ReliablePaneWriteDelivery>> {
        match &self.payload {
            QueuedReliableInputPayload::PaneWrite { delivery, .. } => Some(delivery),
            QueuedReliableInputPayload::Key { .. } => None,
        }
    }

    fn key_request(&self) -> Option<&ReliableKeyEventV1> {
        match &self.payload {
            QueuedReliableInputPayload::Key { request, .. } => Some(request),
            QueuedReliableInputPayload::PaneWrite { .. } => None,
        }
    }

    #[cfg(test)]
    fn key_trace_context(&self) -> Option<SampledTraceContextV1> {
        match &self.payload {
            QueuedReliableInputPayload::Key { trace_context, .. } => *trace_context,
            QueuedReliableInputPayload::PaneWrite { .. } => None,
        }
    }

    fn finish_write(&self, failure: Option<ReliablePaneWriteFailure>) {
        let Some(delivery) = self.write_delivery() else {
            return;
        };
        delivery.finish_chunk(failure);
        if let Some(failure) = failure {
            notify_pane_write_failure(&self.registration, delivery, failure);
        }
    }
}

fn notify_pane_write_failure(
    registration: &PaneRegistrationHandle,
    delivery: &ReliablePaneWriteDelivery,
    failure: ReliablePaneWriteFailure,
) {
    metrics::counter!(
        "mux.client.reliable_pane_write_failure",
        "outcome" => failure.label()
    )
    .increment(1);
    log::error!(
        "reliable pane input failed for pane {}: {}",
        registration.pane_id(),
        failure.message()
    );
    if !delivery.should_notify() {
        return;
    }
    let _ = registration.try_with_current(|current| {
        current.dispatch_alert(Alert::ToastNotification {
            title: Some("FrankenTerm input delivery failed".to_string()),
            body: failure.message().to_string(),
            focus: true,
        });
    });
}

struct ReliableInputQueueState {
    pending: VecDeque<QueuedReliableInput>,
    pending_bytes: usize,
    worker_running: bool,
    domain_detached: bool,
}

pub(crate) struct ReliableInputQueue {
    state: Mutex<ReliableInputQueueState>,
    #[cfg(test)]
    after_claim_generation_barrier: Mutex<Option<ReliableInputClaimGenerationBarrier>>,
    #[cfg(test)]
    after_pane_write_scope_capture_generation_barrier:
        Mutex<Option<ReliableInputClaimGenerationBarrier>>,
    #[cfg(test)]
    after_interactive_scope_capture_generation_barrier:
        Mutex<Option<ReliableInputClaimGenerationBarrier>>,
}

#[cfg(test)]
struct ReliableInputClaimGenerationBarrier {
    peer: crate::client::TestRpcPeer,
    successor_codec_version: usize,
}

enum ReliableInputAttempt {
    Complete(&'static str),
    Progress(&'static str),
    Retry(Duration, &'static str),
    DropOne(&'static str),
    AbortLane(&'static str),
    BindPaneAuthority(ReliablePaneInputAuthority),
    PaneAuthorityRetired(&'static str),
}

enum ReliableInputWireAttempt {
    Unsampled(ReliableKeyEventV1),
    Traced(ReliableKeyEventTracedV1),
}

enum LegacyKeyWireAttempt {
    KeyDown(SendKeyDown),
    KeyUp(SendKeyUp),
}

#[derive(Debug)]
enum ReliableKeyClaimError {
    FifoAuthorityChanged,
    ConnectionIdentityUnavailable,
    ServerRestartAfterAmbiguousAttempt,
    ReliableEffectMayHaveReached,
}

#[derive(Debug)]
enum ReliablePaneWriteClaimError {
    FifoAuthorityChanged,
    ConnectionIdentityUnavailable,
    ServerRestartAfterAmbiguousAttempt,
    ReliableEffectMayHaveReached,
}

fn pane_write_failure_for_outcome(outcome: &'static str) -> ReliablePaneWriteFailure {
    match outcome {
        "unsupported_codec" => ReliablePaneWriteFailure::UnsupportedCodec,
        "client_dropped" | "domain_detached" => ReliablePaneWriteFailure::DomainDetached,
        "server_restart_after_ambiguous_attempt" => {
            ReliablePaneWriteFailure::ServerRestartAfterAmbiguousAttempt
        }
        "outcome_indeterminate" => ReliablePaneWriteFailure::Indeterminate,
        "definitely_not_applied" => ReliablePaneWriteFailure::DefinitelyNotApplied,
        "pane_unavailable" | "pane_registration_mismatch" => ReliablePaneWriteFailure::PaneRetired,
        _ => ReliablePaneWriteFailure::ProtocolError,
    }
}

impl ReliableInputQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ReliableInputQueueState {
                pending: VecDeque::with_capacity(RELIABLE_INPUT_QUEUE_CAPACITY),
                pending_bytes: 0,
                worker_running: false,
                domain_detached: false,
            }),
            #[cfg(test)]
            after_claim_generation_barrier: Mutex::new(None),
            #[cfg(test)]
            after_pane_write_scope_capture_generation_barrier: Mutex::new(None),
            #[cfg(test)]
            after_interactive_scope_capture_generation_barrier: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn arm_after_claim_generation_barrier(
        &self,
        peer: crate::client::TestRpcPeer,
        successor_codec_version: usize,
    ) {
        let mut barrier = self.after_claim_generation_barrier.lock();
        assert!(
            barrier.is_none(),
            "test generation barrier is already armed"
        );
        *barrier = Some(ReliableInputClaimGenerationBarrier {
            peer,
            successor_codec_version,
        });
    }

    #[cfg(test)]
    fn arm_after_pane_write_scope_capture_generation_barrier(
        &self,
        peer: crate::client::TestRpcPeer,
        successor_codec_version: usize,
    ) {
        let mut barrier = self
            .after_pane_write_scope_capture_generation_barrier
            .lock();
        assert!(
            barrier.is_none(),
            "test pane-write scope-capture generation barrier is already armed"
        );
        *barrier = Some(ReliableInputClaimGenerationBarrier {
            peer,
            successor_codec_version,
        });
    }

    #[cfg(test)]
    fn arm_after_interactive_scope_capture_generation_barrier(
        &self,
        peer: crate::client::TestRpcPeer,
        successor_codec_version: usize,
    ) {
        let mut barrier = self
            .after_interactive_scope_capture_generation_barrier
            .lock();
        assert!(
            barrier.is_none(),
            "test interactive scope-capture generation barrier is already armed"
        );
        *barrier = Some(ReliableInputClaimGenerationBarrier {
            peer,
            successor_codec_version,
        });
    }

    #[cfg(test)]
    fn trigger_after_interactive_scope_capture_generation_barrier(
        &self,
        client: &crate::client::Client,
    ) {
        if let Some(barrier) = self
            .after_interactive_scope_capture_generation_barrier
            .lock()
            .take()
        {
            barrier
                .peer
                .replace_ready_generation(client, barrier.successor_codec_version)
                .expect("test barrier must replace the RPC generation after scope capture");
        }
    }

    #[cfg(test)]
    fn enqueue(
        self: &Arc<Self>,
        client: &Arc<ClientInner>,
        registration: PaneRegistrationHandle,
        pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
        request: ReliableKeyEventV1,
    ) -> anyhow::Result<()> {
        self.enqueue_with_trace_context(client, registration, pane_authority, request, None, None)
            .map(|_| ())
    }

    fn enqueue_with_trace_context(
        self: &Arc<Self>,
        client: &Arc<ClientInner>,
        registration: PaneRegistrationHandle,
        pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
        mut request: ReliableKeyEventV1,
        trace_context: Option<SampledTraceContextV1>,
        initial_rpc_scope: Option<RpcGenerationScope>,
    ) -> anyhow::Result<InputSerial> {
        if let Some(trace_context) = trace_context {
            // The queued request intentionally has no pane authority until its
            // no-effect PDU96 probe completes. Validate the content-free
            // context here; the PDU98 validator enforces exact authority once
            // the first effect-eligible wire attempt is constructed.
            validate_sampled_keypress_trace_context(&trace_context)
                .context("validating sampled reliable key input before queue admission")?;
        }
        let (input_serial, worker_reservation) = {
            let mut state = self.state.lock();
            if state.domain_detached || client.is_detached() {
                bail!("cannot enqueue reliable input for a detached client domain");
            }
            if state.pending.len() >= RELIABLE_INPUT_QUEUE_CAPACITY {
                drop(state);
                metrics::counter!("mux.client.reliable_input_queue", "outcome" => "full")
                    .increment(1);
                bail!(
                    "reliable input queue reached its fixed {}-event capacity",
                    RELIABLE_INPUT_QUEUE_CAPACITY
                );
            }
            if state
                .pending_bytes
                .checked_add(RELIABLE_KEY_INPUT_ESTIMATED_BYTES)
                .is_none_or(|bytes| bytes > RELIABLE_INPUT_QUEUE_BYTE_CAPACITY)
            {
                drop(state);
                metrics::counter!("mux.client.reliable_input_queue", "outcome" => "byte_full")
                    .increment(1);
                bail!(
                    "reliable input queue reached its fixed {}-byte capacity",
                    RELIABLE_INPUT_QUEUE_BYTE_CAPACITY
                );
            }
            if request.input_serial.is_empty() {
                request.input_serial = InputSerial::try_now()
                    .ok_or_else(|| anyhow::anyhow!("process-local input serial space exhausted"))?;
            }
            request
                .validate()
                .context("validating reliable key input before queue admission")?;
            let input_serial = request.input_serial;
            let worker_reservation = if state.worker_running {
                None
            } else {
                let reservation = match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Input,
                    64 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                    }
                    rejected => {
                        drop(state);
                        metrics::counter!(
                            "mux.client.reliable_input_queue",
                            "outcome" => "scheduler_rejected"
                        )
                        .increment(1);
                        bail!(
                            "main-thread scheduler rejected reliable-input worker before queue mutation: {rejected:?}"
                        );
                    }
                };
                state.worker_running = true;
                Some(reservation)
            };
            state.pending_bytes += RELIABLE_KEY_INPUT_ESTIMATED_BYTES;
            state.pending.push_back(QueuedReliableInput {
                registration,
                pane_authority,
                payload: QueuedReliableInputPayload::Key {
                    request,
                    trace_context,
                    initial_rpc_scope,
                    attempted_authority: None,
                    effect_may_have_reached: false,
                },
            });
            (input_serial, worker_reservation)
        };
        metrics::counter!("mux.client.reliable_input_queue", "outcome" => "enqueued").increment(1);
        if let Some(reservation) = worker_reservation {
            Self::start_worker_now(Arc::clone(self), Arc::downgrade(client), reservation);
        }
        Ok(input_serial)
    }

    fn enqueue_pane_write(
        self: &Arc<Self>,
        client: &Arc<ClientInner>,
        registration: PaneRegistrationHandle,
        remote_pane_id: PaneId,
        pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
        delivery: Arc<ReliablePaneWriteDelivery>,
        data: &[u8],
    ) -> std::io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let rpc = client.client.rpc_scope();
        let codec_version = rpc.agreed_codec_version();
        if codec_version.is_some_and(|version| {
            version < RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION && version != LEGACY46_CODEC_VERSION
        }) {
            notify_pane_write_failure(
                &registration,
                &delivery,
                ReliablePaneWriteFailure::UnsupportedCodec,
            );
            return Err(std::io::Error::new(
                ReliablePaneWriteFailure::UnsupportedCodec.io_kind(),
                ReliablePaneWriteFailure::UnsupportedCodec.message(),
            ));
        }
        let (accepted, worker_reservation) = {
            let mut state = self.state.lock();
            if state.domain_detached || client.is_detached() {
                drop(state);
                notify_pane_write_failure(
                    &registration,
                    &delivery,
                    ReliablePaneWriteFailure::DomainDetached,
                );
                return Err(std::io::Error::new(
                    ReliablePaneWriteFailure::DomainDetached.io_kind(),
                    ReliablePaneWriteFailure::DomainDetached.message(),
                ));
            }
            let remaining_bytes =
                RELIABLE_INPUT_QUEUE_BYTE_CAPACITY.saturating_sub(state.pending_bytes);
            let payload_budget =
                remaining_bytes.saturating_sub(RELIABLE_PANE_WRITE_ENTRY_OVERHEAD_BYTES);
            if state.pending.len() >= RELIABLE_INPUT_QUEUE_CAPACITY || payload_budget == 0 {
                drop(state);
                notify_pane_write_failure(
                    &registration,
                    &delivery,
                    ReliablePaneWriteFailure::QueueFull,
                );
                return Err(std::io::Error::new(
                    ReliablePaneWriteFailure::QueueFull.io_kind(),
                    ReliablePaneWriteFailure::QueueFull.message(),
                ));
            }
            let accepted = data
                .len()
                .min(MAX_RELIABLE_PANE_WRITE_DATA_BYTES)
                .min(payload_budget);
            let Some((input_serial, reserved_serial_end)) = InputSerial::try_reserve(accepted)
            else {
                return Err(std::io::Error::other(
                    "process-local pane-write serial space exhausted",
                ));
            };
            let request = ReliablePaneWriteV1 {
                pane_id: remote_pane_id,
                pane_registration: None,
                input_serial,
                data: data[..accepted].to_vec(),
            };
            request.validate().map_err(std::io::Error::other)?;
            let worker_reservation = if state.worker_running {
                None
            } else {
                let reservation = match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Input,
                    64 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                    }
                    rejected => {
                        drop(state);
                        notify_pane_write_failure(
                            &registration,
                            &delivery,
                            ReliablePaneWriteFailure::SchedulerRejected,
                        );
                        return Err(std::io::Error::new(
                            ReliablePaneWriteFailure::SchedulerRejected.io_kind(),
                            format!(
                                "{} ({rejected:?})",
                                ReliablePaneWriteFailure::SchedulerRejected.message()
                            ),
                        ));
                    }
                };
                state.worker_running = true;
                Some(reservation)
            };
            if let Err(failure) = delivery.try_accept_chunk() {
                if worker_reservation.is_some() {
                    state.worker_running = false;
                }
                return Err(std::io::Error::new(failure.io_kind(), failure.message()));
            }
            let entry = QueuedReliableInput {
                registration,
                pane_authority,
                payload: QueuedReliableInputPayload::PaneWrite {
                    request,
                    reserved_serial_end,
                    initial_rpc_scope: Some(rpc),
                    attempted_authority: None,
                    effect_may_have_reached: false,
                    delivery,
                },
            };
            state.pending_bytes = state.pending_bytes.saturating_add(entry.estimated_bytes());
            state.pending.push_back(entry);
            (accepted, worker_reservation)
        };
        metrics::counter!("mux.client.reliable_input_queue", "outcome" => "pane_write_enqueued")
            .increment(1);
        if let Some(reservation) = worker_reservation {
            Self::start_worker_now(Arc::clone(self), Arc::downgrade(client), reservation);
        }
        Ok(accepted)
    }

    fn start_worker_now(
        queue: Arc<Self>,
        client: Weak<ClientInner>,
        reservation: promise::spawn::MainThreadSpawnReservation,
    ) {
        let mut worker: Pin<Box<dyn Future<Output = ()>>> = Box::pin(Self::run(queue, client));
        let waker = futures::task::noop_waker();
        let mut context = TaskContext::from_waker(&waker);
        if worker.as_mut().poll(&mut context).is_pending() {
            reservation.spawn_local(worker).detach();
        }
    }

    async fn run(queue: Arc<Self>, client: Weak<ClientInner>) {
        let mut consecutive_retries = 0u32;
        loop {
            let Some(client) = client.upgrade() else {
                queue.retire("client_dropped");
                return;
            };
            if client.is_detached() {
                return;
            }
            let entry = {
                let mut state = queue.state.lock();
                let entry = state.pending.front().cloned();
                if entry.is_none() {
                    // Linearize the drained transition with enqueue. An
                    // enqueue that follows this unlock observes false and
                    // starts the successor worker; an enqueue before it is
                    // visible here and cannot be stranded.
                    state.worker_running = false;
                }
                entry
            };
            let Some(entry) = entry else {
                metrics::counter!(
                    "mux.client.reliable_input_worker",
                    "outcome" => "drained"
                )
                .increment(1);
                return;
            };
            if entry.registration.try_with_current(|_| ()).is_none() {
                if !queue.fail_front(
                    &entry,
                    "pane_registration_retired",
                    ReliablePaneWriteFailure::PaneRetired,
                ) {
                    return;
                }
                consecutive_retries = 0;
                yield_reliable_input_worker_once().await;
                continue;
            }
            if entry.is_pane_write() {
                // `start_worker_now` polls one step synchronously inside the
                // GUI callback. Yield before byte-PDU construction so that
                // codec sizing, compression planning, and transport
                // reservation never run on that callback.
                yield_reliable_input_worker_once().await;
            }
            let attempt = Self::attempt(&queue, &client, &entry).await;
            if client.is_detached() {
                return;
            }
            match attempt {
                ReliableInputAttempt::Complete(outcome) => {
                    if !queue.complete_front(&entry, outcome) {
                        return;
                    }
                    consecutive_retries = 0;
                    yield_reliable_input_worker_once().await;
                }
                ReliableInputAttempt::Progress(outcome) => {
                    metrics::counter!(
                        "mux.client.reliable_input_attempt",
                        "outcome" => outcome
                    )
                    .increment(1);
                    consecutive_retries = 0;
                    yield_reliable_input_worker_once().await;
                }
                ReliableInputAttempt::DropOne(outcome) => {
                    let failure = pane_write_failure_for_outcome(outcome);
                    let settled = if entry.is_pane_write() {
                        queue.fail_pane_write_stream(&entry, outcome, failure)
                    } else {
                        queue.fail_front(&entry, outcome, failure)
                    };
                    if !settled {
                        return;
                    }
                    consecutive_retries = 0;
                    yield_reliable_input_worker_once().await;
                }
                ReliableInputAttempt::Retry(delay, outcome) => {
                    metrics::counter!(
                        "mux.client.reliable_input_attempt",
                        "outcome" => outcome
                    )
                    .increment(1);
                    promise::spawn::sleep(reliable_input_retry_delay(delay, consecutive_retries))
                        .await;
                    consecutive_retries = consecutive_retries.saturating_add(1);
                }
                ReliableInputAttempt::AbortLane(outcome) => {
                    metrics::counter!(
                        "mux.client.reliable_input_attempt",
                        "outcome" => outcome
                    )
                    .increment(1);
                    queue.retire(outcome);
                    return;
                }
                ReliableInputAttempt::BindPaneAuthority(pane_authority) => {
                    if !queue.bind_front_pane_authority(&entry, pane_authority) {
                        queue.retire("pane_authority_conflict");
                        return;
                    }
                    consecutive_retries = 0;
                    yield_reliable_input_worker_once().await;
                }
                ReliableInputAttempt::PaneAuthorityRetired(outcome) => {
                    if !queue.retire_front_pane_authority(&entry, outcome) {
                        queue.retire("pane_authority_retirement_conflict");
                        return;
                    }
                    consecutive_retries = 0;
                    yield_reliable_input_worker_once().await;
                }
            }
        }
    }

    async fn attempt(
        queue: &Self,
        client: &Arc<ClientInner>,
        entry: &QueuedReliableInput,
    ) -> ReliableInputAttempt {
        match &entry.payload {
            QueuedReliableInputPayload::Key { .. } => Self::attempt_key(queue, client, entry).await,
            QueuedReliableInputPayload::PaneWrite { .. } => {
                Self::attempt_pane_write(queue, client, entry).await
            }
        }
    }

    async fn attempt_key(
        queue: &Self,
        client: &Arc<ClientInner>,
        entry: &QueuedReliableInput,
    ) -> ReliableInputAttempt {
        let expected_request = entry
            .key_request()
            .expect("key attempt requires a key queue entry");
        let rpc = match queue.take_front_key_rpc_scope(entry) {
            Ok(Some(rpc)) => rpc,
            Ok(None) => client.client.rpc_scope(),
            Err(()) => return ReliableInputAttempt::AbortLane("fifo_authority_changed"),
        };
        let disposition = reliable_input_codec_disposition(rpc.agreed_codec_version());
        match disposition {
            ReliableInputCodecDisposition::AwaitingAuthority => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "transport_not_ready",
                );
            }
            ReliableInputCodecDisposition::Legacy => {
                return Self::attempt_legacy_key(queue, &rpc, entry).await;
            }
            ReliableInputCodecDisposition::Reliable
            | ReliableInputCodecDisposition::ReliableTraced => {}
        }

        let Some(connection_identity) = rpc.render_connection_identity() else {
            return ReliableInputAttempt::Retry(
                RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                "connection_identity_unavailable",
            );
        };

        let (wire_attempt, consumed_trace_context) = match queue.claim_front_wire_attempt(
            entry,
            disposition,
            connection_identity.session_incarnation,
        ) {
            Ok(attempt) => attempt,
            Err(ReliableKeyClaimError::FifoAuthorityChanged) => {
                return ReliableInputAttempt::AbortLane("fifo_authority_changed");
            }
            Err(ReliableKeyClaimError::ConnectionIdentityUnavailable) => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "connection_identity_unavailable",
                );
            }
            Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt) => {
                return ReliableInputAttempt::DropOne("server_restart_after_ambiguous_attempt");
            }
            Err(ReliableKeyClaimError::ReliableEffectMayHaveReached) => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "codec_downgrade_after_ambiguous_attempt",
                );
            }
        };
        #[cfg(test)]
        if let Some(barrier) = queue.after_claim_generation_barrier.lock().take() {
            barrier
                .peer
                .replace_ready_generation(&client.client, barrier.successor_codec_version)
                .expect("test barrier must replace the RPC generation after the wire claim");
        }
        let request = match &wire_attempt {
            ReliableInputWireAttempt::Unsampled(request) => request,
            ReliableInputWireAttempt::Traced(request) => &request.request,
        };
        let request_had_pane_authority = request.pane_registration.is_some();
        let response = match wire_attempt {
            ReliableInputWireAttempt::Unsampled(request) => {
                rpc.reliable_key_event_v1(request).await
            }
            ReliableInputWireAttempt::Traced(request) => {
                rpc.reliable_key_event_traced_v1(request).await
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) if error.root_cause().is::<crate::client::RpcTransportError>() => {
                let transport = error
                    .root_cause()
                    .downcast_ref::<crate::client::RpcTransportError>()
                    .expect("root-cause type was checked above");
                if transport.delivery_certainty()
                    == crate::client::RpcDeliveryCertainty::DefinitelyNotSent
                {
                    if !queue.restore_front_trace_context(entry, consumed_trace_context) {
                        return ReliableInputAttempt::AbortLane("trace_context_restore_conflict");
                    }
                } else if request_had_pane_authority && !queue.set_front_key_ambiguity(entry, true)
                {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "transport_retired",
                );
            }
            Err(error) if error.root_cause().is::<ClientOutboundAdmissionError>() => {
                if !queue.restore_front_trace_context(entry, consumed_trace_context) {
                    return ReliableInputAttempt::AbortLane("trace_context_restore_conflict");
                }
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "outbound_admission_full",
                );
            }
            Err(error)
                if crate::client::is_definitely_not_sent_reliable_trace_dialect_rejection(
                    &error,
                ) =>
            {
                if !queue.restore_front_trace_context(entry, consumed_trace_context) {
                    return ReliableInputAttempt::AbortLane("trace_context_restore_conflict");
                }
                return ReliableInputAttempt::Retry(
                    Duration::ZERO,
                    "trace_dialect_generation_changed",
                );
            }
            Err(error) => {
                log::warn!("reliable key protocol attempt failed terminally: {error:#}");
                return ReliableInputAttempt::AbortLane("protocol_error");
            }
        };
        if response.pane_id != expected_request.pane_id
            || response.input_serial != expected_request.input_serial
        {
            log::warn!(
                "reliable key response identity mismatch: expected pane={} serial={}, got pane={} serial={}",
                expected_request.pane_id,
                expected_request.input_serial.get(),
                response.pane_id,
                response.input_serial.get(),
            );
            return ReliableInputAttempt::AbortLane("response_identity_mismatch");
        }
        match response.outcome {
            ReliableKeyEventOutcomeV1::Applied => ReliableInputAttempt::Complete("applied"),
            ReliableKeyEventOutcomeV1::DuplicateApplied => {
                ReliableInputAttempt::Complete("duplicate_applied")
            }
            ReliableKeyEventOutcomeV1::Retry(retry) => {
                let (retry_after_ns, outcome, effect_may_have_reached) = match retry {
                    ReliableKeyEventRetryV1::SchedulerFull(pressure) => {
                        (pressure.retry_after_ns, "scheduler_full", false)
                    }
                    ReliableKeyEventRetryV1::SchedulerRetired(pressure) => {
                        (pressure.retry_after_ns, "scheduler_retired", false)
                    }
                    ReliableKeyEventRetryV1::SchedulerUnavailable { retry_after_ns } => {
                        (retry_after_ns, "scheduler_unavailable", false)
                    }
                    ReliableKeyEventRetryV1::DuplicatePending { retry_after_ns } => {
                        (retry_after_ns, "duplicate_pending", true)
                    }
                    ReliableKeyEventRetryV1::ClientRegistrationTransition { retry_after_ns } => {
                        (retry_after_ns, "client_registration_transition", false)
                    }
                    ReliableKeyEventRetryV1::PaneAuthorityRequired { pane_registration } => {
                        if !queue.set_front_key_ambiguity(entry, false) {
                            return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                        }
                        return if !request_had_pane_authority {
                            ReliableInputAttempt::BindPaneAuthority(ReliablePaneInputAuthority {
                                session_incarnation: connection_identity.session_incarnation,
                                pane_registration,
                            })
                        } else {
                            log::warn!(
                                "server requested pane authority after the reliable key request already carried one"
                            );
                            ReliableInputAttempt::AbortLane("repeated_pane_authority_probe")
                        };
                    }
                };
                if !queue.set_front_key_ambiguity(entry, effect_may_have_reached) {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                ReliableInputAttempt::Retry(Duration::from_nanos(retry_after_ns), outcome)
            }
            ReliableKeyEventOutcomeV1::Rejected(rejection) => match rejection {
                ReliableKeyEventRejectionV1::PaneUnavailable => {
                    ReliableInputAttempt::PaneAuthorityRetired("pane_unavailable")
                }
                ReliableKeyEventRejectionV1::PaneRegistrationMismatch => {
                    ReliableInputAttempt::PaneAuthorityRetired("pane_registration_mismatch")
                }
                ReliableKeyEventRejectionV1::ClientLedgerUnavailable => {
                    ReliableInputAttempt::AbortLane("client_ledger_unavailable")
                }
                ReliableKeyEventRejectionV1::IdentityAuthorityExhausted => {
                    ReliableInputAttempt::AbortLane("identity_authority_exhausted")
                }
                ReliableKeyEventRejectionV1::StaleSerial => {
                    ReliableInputAttempt::AbortLane("stale_serial")
                }
                ReliableKeyEventRejectionV1::IdentityConflict => {
                    ReliableInputAttempt::AbortLane("identity_conflict")
                }
                ReliableKeyEventRejectionV1::OutcomeUnknown => {
                    // This serial cannot be replayed safely, but the server's
                    // terminal ledger still permits the next serial. Dropping
                    // only this event lets an already-queued key-up release a
                    // possibly-applied key-down and preserves unrelated panes.
                    ReliableInputAttempt::DropOne("outcome_unknown")
                }
                ReliableKeyEventRejectionV1::InvalidSchedulerConfiguration => {
                    ReliableInputAttempt::AbortLane("invalid_scheduler_configuration")
                }
            },
        }
    }

    async fn attempt_legacy_key(
        queue: &Self,
        rpc: &RpcGenerationScope,
        entry: &QueuedReliableInput,
    ) -> ReliableInputAttempt {
        let Some(connection_generation) = rpc.connection_generation() else {
            return ReliableInputAttempt::Retry(
                RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                "transport_generation_unavailable",
            );
        };
        let (wire_attempt, consumed_trace_context) =
            match queue.claim_front_legacy_key_attempt(entry, connection_generation) {
                Ok(attempt) => attempt,
                Err(ReliableKeyClaimError::FifoAuthorityChanged) => {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                Err(ReliableKeyClaimError::ConnectionIdentityUnavailable) => {
                    return ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "connection_identity_unavailable",
                    );
                }
                Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt) => {
                    return ReliableInputAttempt::DropOne("server_restart_after_ambiguous_attempt");
                }
                Err(ReliableKeyClaimError::ReliableEffectMayHaveReached) => {
                    // A same-server reliable retry remains deduplicable once a
                    // capable generation returns. The legacy dialect has no
                    // serial ledger, so it must not consume ambiguous input.
                    return ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "codec_downgrade_after_ambiguous_attempt",
                    );
                }
            };
        let response = match wire_attempt {
            LegacyKeyWireAttempt::KeyDown(request) => rpc.key_down(request).await,
            LegacyKeyWireAttempt::KeyUp(request) => rpc.key_up(request).await,
        };
        match response {
            Ok(_) => ReliableInputAttempt::Complete("legacy_applied"),
            Err(error) if error.root_cause().is::<crate::client::RpcTransportError>() => {
                let transport = error
                    .root_cause()
                    .downcast_ref::<crate::client::RpcTransportError>()
                    .expect("root-cause type was checked above");
                if transport.delivery_certainty()
                    == crate::client::RpcDeliveryCertainty::DefinitelyNotSent
                {
                    if !queue.restore_front_trace_context(entry, consumed_trace_context) {
                        return ReliableInputAttempt::AbortLane("trace_context_restore_conflict");
                    }
                    ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "transport_retired",
                    )
                } else {
                    // Legacy key PDUs have no replay ledger. Once physical
                    // delivery is uncertain, at-most-once requires settling
                    // this event without a retry.
                    ReliableInputAttempt::DropOne("outcome_indeterminate")
                }
            }
            Err(error) if error.root_cause().is::<ClientOutboundAdmissionError>() => {
                if !queue.restore_front_trace_context(entry, consumed_trace_context) {
                    return ReliableInputAttempt::AbortLane("trace_context_restore_conflict");
                }
                ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "outbound_admission_full",
                )
            }
            Err(error) => {
                log::warn!("legacy key attempt failed terminally: {error:#}");
                ReliableInputAttempt::DropOne("protocol_error")
            }
        }
    }

    async fn attempt_pane_write(
        queue: &Self,
        client: &Arc<ClientInner>,
        entry: &QueuedReliableInput,
    ) -> ReliableInputAttempt {
        let rpc = match queue.take_front_pane_write_rpc_scope(entry) {
            Ok(Some(rpc)) => rpc,
            Ok(None) => client.client.rpc_scope(),
            Err(()) => return ReliableInputAttempt::AbortLane("fifo_authority_changed"),
        };
        #[cfg(test)]
        if let Some(barrier) = queue
            .after_pane_write_scope_capture_generation_barrier
            .lock()
            .take()
        {
            barrier
                .peer
                .replace_ready_generation(&client.client, barrier.successor_codec_version)
                .expect("test barrier must replace the RPC generation after scope capture");
        }
        match rpc.agreed_codec_version() {
            None => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "transport_not_ready",
                );
            }
            Some(LEGACY46_CODEC_VERSION) => {
                return Self::attempt_legacy_pane_write(queue, &rpc, entry).await;
            }
            Some(_) => {}
        }
        let Some(connection_identity) = rpc.render_connection_identity() else {
            return ReliableInputAttempt::Retry(
                RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                "connection_identity_unavailable",
            );
        };
        let request = match queue
            .claim_front_pane_write_attempt(entry, connection_identity.session_incarnation)
        {
            Ok(request) => request,
            Err(ReliablePaneWriteClaimError::FifoAuthorityChanged) => {
                return ReliableInputAttempt::AbortLane("fifo_authority_changed");
            }
            Err(ReliablePaneWriteClaimError::ConnectionIdentityUnavailable) => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "connection_identity_unavailable",
                );
            }
            Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt) => {
                return ReliableInputAttempt::DropOne("server_restart_after_ambiguous_attempt");
            }
            Err(ReliablePaneWriteClaimError::ReliableEffectMayHaveReached) => {
                return ReliableInputAttempt::DropOne("outcome_indeterminate");
            }
        };
        let request_had_pane_authority = request.pane_registration.is_some();
        let expected_pane_id = request.pane_id;
        let expected_input_serial = request.input_serial;
        let expected_data_len = request.data.len();
        let response = rpc.reliable_pane_write_v1(request).await;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.root_cause().is::<crate::client::RpcTransportError>() => {
                let transport = error
                    .root_cause()
                    .downcast_ref::<crate::client::RpcTransportError>()
                    .expect("root-cause type was checked above");
                if request_had_pane_authority
                    && transport.delivery_certainty()
                        == crate::client::RpcDeliveryCertainty::OutcomeUnknown
                    && !queue.set_front_pane_write_ambiguity(entry, true)
                {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "transport_retired",
                );
            }
            Err(error) if error.root_cause().is::<ClientOutboundAdmissionError>() => {
                return ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "outbound_admission_full",
                );
            }
            Err(error)
                if crate::client::is_definitely_not_sent_reliable_pane_write_dialect_rejection(
                    &error,
                ) =>
            {
                return if rpc.same_generation(&client.client.rpc_scope()) {
                    // The queue already owns these bytes, but exact dialect
                    // validation proves that none crossed the wire. Retain
                    // them for an automatic capable-generation upgrade.
                    ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "unsupported_codec",
                    )
                } else {
                    ReliableInputAttempt::Retry(
                        Duration::ZERO,
                        "pane_write_dialect_generation_changed",
                    )
                };
            }
            Err(error) => {
                log::warn!("reliable pane-write protocol attempt failed terminally: {error:#}");
                return ReliableInputAttempt::DropOne("protocol_error");
            }
        };
        if response.pane_id != expected_pane_id || response.input_serial != expected_input_serial {
            log::warn!(
                "reliable pane-write response identity mismatch: expected pane={} serial={}, got pane={} serial={}",
                expected_pane_id,
                expected_input_serial.get(),
                response.pane_id,
                response.input_serial.get(),
            );
            return ReliableInputAttempt::DropOne("response_identity_mismatch");
        }

        match response.outcome {
            ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes }
            | ReliablePaneWriteOutcomeV1::DuplicateAppliedPrefix { bytes } => {
                let Ok(bytes) = usize::try_from(bytes) else {
                    return ReliableInputAttempt::DropOne("outcome_indeterminate");
                };
                if bytes == 0 || bytes > expected_data_len {
                    log::warn!(
                        "reliable pane-write response claimed prefix {bytes} for exact {}-byte request",
                        expected_data_len
                    );
                    return ReliableInputAttempt::DropOne("outcome_indeterminate");
                }
                match queue.apply_front_pane_write_prefix(entry, bytes) {
                    Ok(true) => ReliableInputAttempt::Complete("applied_prefix"),
                    Ok(false) => ReliableInputAttempt::Progress("partial_prefix_applied"),
                    Err(()) => ReliableInputAttempt::DropOne("outcome_indeterminate"),
                }
            }
            ReliablePaneWriteOutcomeV1::Retry(retry) => {
                let (retry_after_ns, outcome, effect_may_have_reached) = match retry {
                    ReliablePaneWriteRetryV1::Input(ReliableKeyEventRetryV1::SchedulerFull(
                        pressure,
                    )) => (pressure.retry_after_ns, "scheduler_full", false),
                    ReliablePaneWriteRetryV1::Input(ReliableKeyEventRetryV1::SchedulerRetired(
                        pressure,
                    )) => (pressure.retry_after_ns, "scheduler_retired", false),
                    ReliablePaneWriteRetryV1::Input(
                        ReliableKeyEventRetryV1::SchedulerUnavailable { retry_after_ns },
                    ) => (retry_after_ns, "scheduler_unavailable", false),
                    ReliablePaneWriteRetryV1::Input(
                        ReliableKeyEventRetryV1::DuplicatePending { retry_after_ns },
                    ) => (retry_after_ns, "duplicate_pending", true),
                    ReliablePaneWriteRetryV1::Input(
                        ReliableKeyEventRetryV1::ClientRegistrationTransition { retry_after_ns },
                    ) => (retry_after_ns, "client_registration_transition", false),
                    ReliablePaneWriteRetryV1::Input(
                        ReliableKeyEventRetryV1::PaneAuthorityRequired { pane_registration },
                    ) => {
                        return if !request_had_pane_authority {
                            ReliableInputAttempt::BindPaneAuthority(ReliablePaneInputAuthority {
                                session_incarnation: connection_identity.session_incarnation,
                                pane_registration,
                            })
                        } else {
                            ReliableInputAttempt::DropOne("repeated_pane_authority_probe")
                        };
                    }
                    ReliablePaneWriteRetryV1::DefinitelyNotApplied { retry_after_ns } => {
                        (retry_after_ns, "write_zero", false)
                    }
                };
                if !queue.set_front_pane_write_ambiguity(entry, effect_may_have_reached) {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                ReliableInputAttempt::Retry(Duration::from_nanos(retry_after_ns), outcome)
            }
            ReliablePaneWriteOutcomeV1::Rejected(rejection) => match rejection {
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::PaneUnavailable,
                ) => ReliableInputAttempt::PaneAuthorityRetired("pane_unavailable"),
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::PaneRegistrationMismatch,
                ) => ReliableInputAttempt::PaneAuthorityRetired("pane_registration_mismatch"),
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::OutcomeUnknown,
                ) => ReliableInputAttempt::DropOne("outcome_indeterminate"),
                ReliablePaneWriteRejectionV1::DefinitelyNotApplied => {
                    ReliableInputAttempt::DropOne("definitely_not_applied")
                }
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::ClientLedgerUnavailable,
                ) => ReliableInputAttempt::AbortLane("client_ledger_unavailable"),
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::IdentityAuthorityExhausted,
                ) => ReliableInputAttempt::AbortLane("identity_authority_exhausted"),
                ReliablePaneWriteRejectionV1::Input(ReliableKeyEventRejectionV1::StaleSerial) => {
                    ReliableInputAttempt::AbortLane("stale_serial")
                }
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::IdentityConflict,
                ) => ReliableInputAttempt::AbortLane("identity_conflict"),
                ReliablePaneWriteRejectionV1::Input(
                    ReliableKeyEventRejectionV1::InvalidSchedulerConfiguration,
                ) => ReliableInputAttempt::AbortLane("invalid_scheduler_configuration"),
            },
            ReliablePaneWriteOutcomeV1::Indeterminate => {
                ReliableInputAttempt::DropOne("outcome_indeterminate")
            }
        }
    }

    async fn attempt_legacy_pane_write(
        queue: &Self,
        rpc: &RpcGenerationScope,
        entry: &QueuedReliableInput,
    ) -> ReliableInputAttempt {
        let Some(connection_generation) = rpc.connection_generation() else {
            return ReliableInputAttempt::Retry(
                RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                "transport_generation_unavailable",
            );
        };
        let request =
            match queue.claim_front_legacy_pane_write_attempt(entry, connection_generation) {
                Ok(request) => request,
                Err(ReliablePaneWriteClaimError::FifoAuthorityChanged) => {
                    return ReliableInputAttempt::AbortLane("fifo_authority_changed");
                }
                Err(ReliablePaneWriteClaimError::ConnectionIdentityUnavailable) => {
                    return ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "connection_identity_unavailable",
                    );
                }
                Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt) => {
                    return ReliableInputAttempt::DropOne("server_restart_after_ambiguous_attempt");
                }
                Err(ReliablePaneWriteClaimError::ReliableEffectMayHaveReached) => {
                    return ReliableInputAttempt::DropOne("outcome_indeterminate");
                }
            };

        match rpc.write_to_pane(request).await {
            Ok(_) => ReliableInputAttempt::Complete("legacy_applied"),
            Err(error) if error.root_cause().is::<crate::client::RpcTransportError>() => {
                let transport = error
                    .root_cause()
                    .downcast_ref::<crate::client::RpcTransportError>()
                    .expect("root-cause type was checked above");
                if transport.delivery_certainty()
                    == crate::client::RpcDeliveryCertainty::DefinitelyNotSent
                {
                    ReliableInputAttempt::Retry(
                        RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                        "transport_retired",
                    )
                } else {
                    // Codec 46 has no server-side input ledger. Once the
                    // physical transport may have carried PDU9, replaying it
                    // could duplicate terminal bytes.
                    ReliableInputAttempt::DropOne("outcome_indeterminate")
                }
            }
            Err(error) if error.root_cause().is::<ClientOutboundAdmissionError>() => {
                ReliableInputAttempt::Retry(
                    RELIABLE_INPUT_TRANSPORT_RETRY_DELAY,
                    "outbound_admission_full",
                )
            }
            Err(error) => {
                log::warn!("legacy pane-write attempt failed terminally: {error:#}");
                ReliableInputAttempt::DropOne("outcome_indeterminate")
            }
        }
    }

    fn claim_front_pane_write_attempt(
        &self,
        expected: &QueuedReliableInput,
        current_session: MuxSessionIncarnation,
    ) -> Result<ReliablePaneWriteV1, ReliablePaneWriteClaimError> {
        if current_session.as_bytes() == [0; 16] {
            return Err(ReliablePaneWriteClaimError::ConnectionIdentityUnavailable);
        }
        let current_authority = ReliableInputAttemptAuthority::ServerSession(current_session);
        let mut pane_authority = expected.pane_authority.lock();
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        };
        if !front.same_identity(expected) {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        }
        let QueuedReliableInputPayload::PaneWrite {
            request,
            attempted_authority,
            effect_may_have_reached,
            ..
        } = &mut front.payload
        else {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        };
        if pane_authority
            .as_ref()
            .is_some_and(|authority| authority.session_incarnation != current_session)
        {
            *pane_authority = None;
        }
        if attempted_authority.is_some_and(|attempted| attempted != current_authority) {
            if *effect_may_have_reached {
                return Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt);
            }
            *pane_authority = None;
        }
        *attempted_authority = Some(current_authority);
        let mut wire_request = request.clone();
        wire_request.pane_registration = pane_authority
            .as_ref()
            .map(|authority| authority.pane_registration);
        Ok(wire_request)
    }

    fn claim_front_legacy_pane_write_attempt(
        &self,
        expected: &QueuedReliableInput,
        connection_generation: NonZeroU64,
    ) -> Result<WriteToPane, ReliablePaneWriteClaimError> {
        let current_authority =
            ReliableInputAttemptAuthority::LegacyTransport(connection_generation);
        let mut pane_authority = expected.pane_authority.lock();
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        };
        if !front.same_identity(expected) {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        }
        let QueuedReliableInputPayload::PaneWrite {
            request,
            attempted_authority,
            effect_may_have_reached,
            ..
        } = &mut front.payload
        else {
            return Err(ReliablePaneWriteClaimError::FifoAuthorityChanged);
        };
        if *effect_may_have_reached {
            return if attempted_authority.is_some_and(|attempted| attempted != current_authority) {
                Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt)
            } else {
                Err(ReliablePaneWriteClaimError::ReliableEffectMayHaveReached)
            };
        }
        // Codec 46 has no durable pane-registration or input-effect ledger.
        // Fence its single non-idempotent attempt to the exact physical
        // transport and never carry a modern server-session authority into it.
        *pane_authority = None;
        *attempted_authority = Some(current_authority);
        Ok(WriteToPane {
            pane_id: request.pane_id,
            data: request.data.clone(),
        })
    }

    fn set_front_pane_write_ambiguity(
        &self,
        expected: &QueuedReliableInput,
        effect_may_have_reached: bool,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return false;
        };
        if !front.same_identity(expected) {
            return false;
        }
        let QueuedReliableInputPayload::PaneWrite {
            effect_may_have_reached: front_ambiguity,
            ..
        } = &mut front.payload
        else {
            return false;
        };
        *front_ambiguity = effect_may_have_reached;
        true
    }

    fn take_front_key_rpc_scope(
        &self,
        expected: &QueuedReliableInput,
    ) -> Result<Option<RpcGenerationScope>, ()> {
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(());
        };
        if !front.same_identity(expected) {
            return Err(());
        }
        let QueuedReliableInputPayload::Key {
            initial_rpc_scope, ..
        } = &mut front.payload
        else {
            return Err(());
        };
        Ok(initial_rpc_scope.take())
    }

    fn take_front_pane_write_rpc_scope(
        &self,
        expected: &QueuedReliableInput,
    ) -> Result<Option<RpcGenerationScope>, ()> {
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(());
        };
        if !front.same_identity(expected) {
            return Err(());
        }
        let QueuedReliableInputPayload::PaneWrite {
            initial_rpc_scope, ..
        } = &mut front.payload
        else {
            return Err(());
        };
        Ok(initial_rpc_scope.take())
    }

    fn set_front_key_ambiguity(
        &self,
        expected: &QueuedReliableInput,
        effect_may_have_reached: bool,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return false;
        };
        if !front.same_identity(expected) {
            return false;
        }
        let QueuedReliableInputPayload::Key {
            effect_may_have_reached: front_ambiguity,
            ..
        } = &mut front.payload
        else {
            return false;
        };
        *front_ambiguity = effect_may_have_reached;
        true
    }

    fn claim_front_legacy_key_attempt(
        &self,
        expected: &QueuedReliableInput,
        connection_generation: NonZeroU64,
    ) -> Result<(LegacyKeyWireAttempt, Option<SampledTraceContextV1>), ReliableKeyClaimError> {
        let current_authority =
            ReliableInputAttemptAuthority::LegacyTransport(connection_generation);
        let mut pane_authority = expected.pane_authority.lock();
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        };
        if !front.same_identity(expected) {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        }
        let QueuedReliableInputPayload::Key {
            request,
            trace_context,
            attempted_authority,
            effect_may_have_reached,
            ..
        } = &mut front.payload
        else {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        };
        if *effect_may_have_reached {
            return if attempted_authority.is_some_and(|attempted| attempted != current_authority) {
                Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt)
            } else {
                Err(ReliableKeyClaimError::ReliableEffectMayHaveReached)
            };
        }
        // Codec-46 has neither a pane-registration ledger nor a durable
        // server-session identity. Never carry modern authority into its
        // non-idempotent key path, and bind this one attempt to the exact
        // physical transport generation instead.
        *pane_authority = None;
        *attempted_authority = Some(current_authority);
        let consumed_trace_context = trace_context.take();
        let wire_attempt = match request.kind {
            ReliableKeyEventKindV1::KeyDown => LegacyKeyWireAttempt::KeyDown(SendKeyDown {
                pane_id: request.pane_id,
                event: request.event.clone(),
                input_serial: request.input_serial,
            }),
            ReliableKeyEventKindV1::KeyUp => LegacyKeyWireAttempt::KeyUp(SendKeyUp {
                pane_id: request.pane_id,
                event: request.event.clone(),
            }),
        };
        Ok((wire_attempt, consumed_trace_context))
    }

    /// Retire one proven applied prefix in place. `Ok(false)` means a suffix
    /// remains at the FIFO front under its next pre-reserved serial; `Ok(true)`
    /// means the caller may remove the fully settled entry.
    fn apply_front_pane_write_prefix(
        &self,
        expected: &QueuedReliableInput,
        applied_bytes: usize,
    ) -> Result<bool, ()> {
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(());
        };
        if !front.same_identity(expected) {
            return Err(());
        }
        let QueuedReliableInputPayload::PaneWrite {
            request,
            reserved_serial_end,
            effect_may_have_reached,
            ..
        } = &mut front.payload
        else {
            return Err(());
        };
        if applied_bytes == 0 || applied_bytes > request.data.len() {
            return Err(());
        }
        if applied_bytes == request.data.len() {
            return Ok(true);
        }
        let next_serial = request
            .input_serial
            .get()
            .checked_add(u64::try_from(applied_bytes).map_err(|_| ())?)
            .ok_or(())?;
        let remaining_bytes = request.data.len().checked_sub(applied_bytes).ok_or(())?;
        let suffix_end = next_serial
            .checked_add(
                u64::try_from(remaining_bytes)
                    .map_err(|_| ())?
                    .checked_sub(1)
                    .ok_or(())?,
            )
            .ok_or(())?;
        if suffix_end != reserved_serial_end.get() {
            return Err(());
        }
        request.data = request.data.split_off(applied_bytes);
        request.input_serial = InputSerial::from_millis_since_epoch(next_serial);
        request.pane_registration = None;
        // Preserve the server incarnation that acknowledged the prefix.  A
        // same-server suffix may reuse its pane authority; if the server is
        // replaced before the suffix attempt, `claim_front_pane_write_attempt`
        // sees the incarnation change and safely re-probes instead of sending
        // a stale registration or quarantining a proven-unapplied suffix.
        *effect_may_have_reached = false;
        state.pending_bytes = state.pending_bytes.checked_sub(applied_bytes).ok_or(())?;
        Ok(false)
    }

    fn claim_front_wire_attempt(
        &self,
        expected: &QueuedReliableInput,
        disposition: ReliableInputCodecDisposition,
        current_session: MuxSessionIncarnation,
    ) -> Result<(ReliableInputWireAttempt, Option<SampledTraceContextV1>), ReliableKeyClaimError>
    {
        if current_session.as_bytes() == [0; 16] {
            return Err(ReliableKeyClaimError::ConnectionIdentityUnavailable);
        }
        let current_authority = ReliableInputAttemptAuthority::ServerSession(current_session);
        let mut pane_authority = expected.pane_authority.lock();
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        };
        if !front.same_identity(expected) {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        }
        let QueuedReliableInputPayload::Key {
            request,
            trace_context,
            attempted_authority,
            effect_may_have_reached,
            ..
        } = &mut front.payload
        else {
            return Err(ReliableKeyClaimError::FifoAuthorityChanged);
        };
        if attempted_authority.is_some_and(|attempted| attempted != current_authority)
            && *effect_may_have_reached
        {
            return Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt);
        }
        if pane_authority
            .as_ref()
            .is_some_and(|authority| authority.session_incarnation != current_session)
            || attempted_authority.is_some_and(|attempted| attempted != current_authority)
        {
            *pane_authority = None;
        }
        *attempted_authority = Some(current_authority);
        let pane_registration = pane_authority
            .as_ref()
            .map(|authority| authority.pane_registration);
        let mut request = request.clone();
        request.pane_registration = pane_registration;
        // A missing pane-registration identity is a no-effect authority probe,
        // not the first effect-eligible wire attempt. Keep the sampled context
        // queued until the exact authority is bound. Once an authority-bound
        // attempt can reach the pane callback, consume the context exactly
        // once even if this peer is too old for PDU98: a later reconnect must
        // never add K4/K5 to an ambiguous v61 retry.
        let trace_context = if request.pane_registration.is_some() {
            trace_context.take()
        } else {
            None
        };
        let attempt = match (disposition, trace_context) {
            (ReliableInputCodecDisposition::ReliableTraced, Some(trace_context)) => {
                ReliableInputWireAttempt::Traced(ReliableKeyEventTracedV1 {
                    request,
                    trace_context,
                })
            }
            _ => ReliableInputWireAttempt::Unsampled(request),
        };
        Ok((attempt, trace_context))
    }

    fn restore_front_trace_context(
        &self,
        expected: &QueuedReliableInput,
        trace_context: Option<SampledTraceContextV1>,
    ) -> bool {
        let Some(trace_context) = trace_context else {
            return true;
        };
        let mut state = self.state.lock();
        let Some(front) = state.pending.front_mut() else {
            return false;
        };
        if !front.same_identity(expected) {
            return false;
        }
        let QueuedReliableInputPayload::Key {
            trace_context: front_trace_context,
            ..
        } = &mut front.payload
        else {
            return false;
        };
        if front_trace_context.is_some() {
            return false;
        }
        *front_trace_context = Some(trace_context);
        true
    }

    fn complete_front(&self, expected: &QueuedReliableInput, outcome: &'static str) -> bool {
        self.remove_front(expected, outcome, None)
    }

    fn fail_front(
        &self,
        expected: &QueuedReliableInput,
        outcome: &'static str,
        failure: ReliablePaneWriteFailure,
    ) -> bool {
        self.remove_front(expected, outcome, Some(failure))
    }

    /// A terminal byte-stream failure makes every later chunk already owned
    /// by that same `PaneWriter` unsafe to deliver: applying them after a
    /// rejected or indeterminate predecessor would violate stream order. Drop
    /// those chunks together while leaving reliable keys (notably key-up) and
    /// unrelated pane writers in their original FIFO order.
    fn fail_pane_write_stream(
        &self,
        expected: &QueuedReliableInput,
        outcome: &'static str,
        failure: ReliablePaneWriteFailure,
    ) -> bool {
        let Some(expected_delivery) = expected.write_delivery() else {
            return false;
        };
        let mut state = self.state.lock();
        if !state
            .pending
            .front()
            .is_some_and(|front| front.same_identity(expected))
        {
            return false;
        }
        let mut retained = VecDeque::with_capacity(state.pending.len());
        let mut removed = Vec::new();
        while let Some(queued) = state.pending.pop_front() {
            if queued
                .write_delivery()
                .is_some_and(|delivery| Arc::ptr_eq(delivery, expected_delivery))
            {
                removed.push(queued);
            } else {
                retained.push_back(queued);
            }
        }
        state.pending = retained;
        state.pending_bytes = state
            .pending
            .iter()
            .map(QueuedReliableInput::estimated_bytes)
            .sum();
        drop(state);
        for queued in removed {
            queued.finish_write(Some(failure));
        }
        metrics::counter!("mux.client.reliable_input_attempt", "outcome" => outcome).increment(1);
        true
    }

    fn remove_front(
        &self,
        expected: &QueuedReliableInput,
        outcome: &'static str,
        failure: Option<ReliablePaneWriteFailure>,
    ) -> bool {
        let mut state = self.state.lock();
        let matches = state
            .pending
            .front()
            .is_some_and(|front| front.same_identity(expected));
        if !matches {
            let domain_detached = state.domain_detached;
            state.worker_running = false;
            drop(state);
            if domain_detached {
                return false;
            }
            log::error!(
                "reliable input FIFO authority changed while completing serial {}",
                expected.input_serial().get()
            );
            return false;
        }
        let removed = state
            .pending
            .pop_front()
            .expect("matched reliable-input front must remain present");
        state.pending_bytes = state
            .pending_bytes
            .saturating_sub(removed.estimated_bytes());
        drop(state);
        removed.finish_write(failure);
        metrics::counter!("mux.client.reliable_input_attempt", "outcome" => outcome).increment(1);
        true
    }

    fn bind_front_pane_authority(
        &self,
        expected: &QueuedReliableInput,
        pane_authority_binding: ReliablePaneInputAuthority,
    ) -> bool {
        let mut pane_authority = expected.pane_authority.lock();
        if pane_authority.is_some_and(|current| current != pane_authority_binding) {
            return false;
        }
        let state = self.state.lock();
        let matches = state
            .pending
            .front()
            .is_some_and(|front| front.same_identity(expected));
        if !matches {
            return false;
        }
        *pane_authority = Some(pane_authority_binding);
        true
    }

    fn retire_front_pane_authority(
        &self,
        expected: &QueuedReliableInput,
        outcome: &'static str,
    ) -> bool {
        let mut pane_authority = expected.pane_authority.lock();
        let mut state = self.state.lock();
        let matches = state
            .pending
            .front()
            .is_some_and(|front| front.same_identity(expected));
        if !matches {
            return false;
        }
        let mut retained = VecDeque::with_capacity(state.pending.len());
        let mut removed = Vec::new();
        while let Some(queued) = state.pending.pop_front() {
            if queued
                .registration
                .same_registration(&expected.registration)
                && Arc::ptr_eq(&queued.pane_authority, &expected.pane_authority)
            {
                removed.push(queued);
            } else {
                retained.push_back(queued);
            }
        }
        state.pending = retained;
        state.pending_bytes = state
            .pending
            .iter()
            .map(QueuedReliableInput::estimated_bytes)
            .sum();
        *pane_authority = None;
        drop(state);
        drop(pane_authority);
        for queued in removed {
            queued.finish_write(Some(ReliablePaneWriteFailure::PaneRetired));
        }
        metrics::counter!("mux.client.reliable_input_attempt", "outcome" => outcome).increment(1);
        true
    }

    pub(crate) fn retire(&self, outcome: &'static str) {
        let mut state = self.state.lock();
        let pending = std::mem::take(&mut state.pending);
        state.pending_bytes = 0;
        state.worker_running = false;
        drop(state);
        let failure = pane_write_failure_for_outcome(outcome);
        for queued in pending {
            queued.finish_write(Some(failure));
        }
        metrics::counter!("mux.client.reliable_input_worker", "outcome" => outcome).increment(1);
    }

    pub(crate) fn detach_domain(&self, detached: &std::sync::atomic::AtomicBool) {
        use std::sync::atomic::Ordering;

        let mut state = self.state.lock();
        if detached.swap(true, Ordering::AcqRel) {
            return;
        }
        state.domain_detached = true;
        let pending = std::mem::take(&mut state.pending);
        state.pending_bytes = 0;
        state.worker_running = false;
        drop(state);
        for queued in pending {
            queued.finish_write(Some(ReliablePaneWriteFailure::DomainDetached));
        }
        metrics::counter!(
            "mux.client.reliable_input_worker",
            "outcome" => "domain_detached"
        )
        .increment(1);
    }
}

fn should_process_unilateral_render_delta(
    current_seqno: SequenceNo,
    incoming_seqno: SequenceNo,
    input_dispatch_serial: Option<InputSerial>,
) -> bool {
    incoming_seqno >= current_seqno || input_dispatch_serial.is_some()
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
#[derive(Clone, Copy, Debug)]
struct ClientRenderApplicationLimits {
    dirty_ranges: usize,
    lines: usize,
    cells: usize,
    hyperlink_spans: usize,
    image_references: usize,
    image_bytes: usize,
    semantic_zones: usize,
    semantic_text_bytes: usize,
    alert_text_bytes: usize,
    title_bytes: usize,
    working_dir_bytes: usize,
    scrollback_rows: usize,
    viewport_cells: usize,
    supports_images: bool,
    supports_semantic_zones: bool,
    supports_palette: bool,
    supports_alerts: bool,
}

impl Default for ClientRenderApplicationLimits {
    fn default() -> Self {
        Self {
            dirty_ranges: MAX_RENDER_APPLICATION_DIRTY_RANGES,
            lines: MAX_RENDER_APPLICATION_LINES,
            cells: MAX_RENDER_APPLICATION_CELLS,
            hyperlink_spans: MAX_RENDER_APPLICATION_HYPERLINK_SPANS,
            image_references: MAX_RENDER_APPLICATION_IMAGE_REFERENCES,
            image_bytes: MAX_RENDER_APPLICATION_IMAGE_BYTES,
            semantic_zones: MAX_RENDER_APPLICATION_SEMANTIC_ZONES,
            semantic_text_bytes: MAX_RENDER_APPLICATION_SEMANTIC_TEXT_BYTES,
            alert_text_bytes: MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES,
            title_bytes: MAX_RENDER_APPLICATION_TITLE_BYTES,
            working_dir_bytes: MAX_RENDER_APPLICATION_WORKING_DIR_BYTES,
            scrollback_rows: MAX_RENDER_APPLICATION_SCROLLBACK_ROWS,
            viewport_cells: MAX_RENDER_APPLICATION_CELLS,
            supports_images: true,
            supports_semantic_zones: true,
            supports_palette: true,
            supports_alerts: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderApplicationLogicalIdentity {
    connection_identity: RenderConnectionIdentity,
    connection_generation: u64,
    coordinator_instance: u64,
    scheduler_sequence: u64,
    ledger_instance: u64,
    render_generation: u64,
    ledger_obligation: u64,
    pane_id: PaneId,
    base_state: Option<RenderStateIdentity>,
    resulting_state: RenderStateIdentity,
    kind: RenderApplicationKind,
}

impl RenderApplicationLogicalIdentity {
    fn new(
        connection_identity: RenderConnectionIdentity,
        identity: RenderApplicationIdentity,
    ) -> Self {
        Self {
            connection_identity,
            connection_generation: identity.token.connection_generation,
            coordinator_instance: identity.token.coordinator_instance,
            scheduler_sequence: identity.token.scheduler_sequence,
            ledger_instance: identity.token.ledger_instance,
            render_generation: identity.token.render_generation,
            ledger_obligation: identity.token.ledger_obligation,
            pane_id: identity.pane_id,
            base_state: identity.base_state,
            resulting_state: identity.resulting_state,
            kind: identity.kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ClientRenderApplicationCounters {
    pub applications_started: u64,
    pub acknowledgements: u64,
    pub duplicate_acknowledgements: u64,
    pub duplicate_in_progress: u64,
    pub nacks: u64,
    pub cancelled_attempts: u64,
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
#[derive(Default)]
struct ClientRenderApplicationState {
    active_connection_identity: Option<RenderConnectionIdentity>,
    active_connection_generation: Option<u64>,
    applied_connection_identity: Option<RenderConnectionIdentity>,
    applied_connection_generation: Option<u64>,
    applied_state: Option<RenderStateIdentity>,
    last_applied: Option<RenderApplicationLogicalIdentity>,
    applying: Option<RenderApplicationLogicalIdentity>,
    counters: ClientRenderApplicationCounters,
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
enum ClientRenderApplicationBegin {
    Apply,
    DuplicateApplied,
    DuplicateInProgress,
    Nack {
        reason: RenderApplicationNackReason,
        observed_state: RenderApplicationObservedState,
    },
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
impl ClientRenderApplicationState {
    fn prepare_authoritative_bootstrap(
        &mut self,
        connection_identity: RenderConnectionIdentity,
        connection_generation: u64,
    ) -> bool {
        if self.active_connection_identity == Some(connection_identity)
            && self.active_connection_generation == Some(connection_generation)
        {
            return false;
        }

        self.active_connection_identity = Some(connection_identity);
        self.active_connection_generation = Some(connection_generation);
        self.applied_connection_identity = None;
        self.applied_connection_generation = None;
        self.applied_state = None;
        self.last_applied = None;
        if self.applying.take().is_some() {
            self.counters.cancelled_attempts = self.counters.cancelled_attempts.saturating_add(1);
        }
        true
    }

    fn observation(&self) -> RenderApplicationObservedState {
        self.applied_state.map_or(
            RenderApplicationObservedState::Uninitialized,
            RenderApplicationObservedState::Applied,
        )
    }

    fn begin(
        &mut self,
        expected_connection_identity: Option<RenderConnectionIdentity>,
        expected_connection_generation: Option<u64>,
        expected_pane_id: PaneId,
        connection_identity: RenderConnectionIdentity,
        identity: RenderApplicationIdentity,
    ) -> ClientRenderApplicationBegin {
        if expected_connection_identity != Some(connection_identity)
            || expected_connection_generation != Some(identity.token.connection_generation)
        {
            return ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                observed_state: self.observation(),
            };
        }
        if identity.pane_id != expected_pane_id {
            return ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::MalformedOrIncomplete {
                    component: RenderApplicationComponent::Surface,
                },
                observed_state: RenderApplicationObservedState::NotApplicable,
            };
        }
        if self.active_connection_identity != Some(connection_identity)
            || self.active_connection_generation != Some(identity.token.connection_generation)
        {
            return ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                observed_state: RenderApplicationObservedState::NotApplicable,
            };
        }

        let logical_identity = RenderApplicationLogicalIdentity::new(connection_identity, identity);
        if self.last_applied == Some(logical_identity)
            && self.applied_state == Some(identity.resulting_state)
        {
            self.counters.duplicate_acknowledgements =
                self.counters.duplicate_acknowledgements.saturating_add(1);
            return ClientRenderApplicationBegin::DuplicateApplied;
        }
        if let Some(applying) = self.applying {
            if applying == logical_identity {
                self.counters.duplicate_in_progress =
                    self.counters.duplicate_in_progress.saturating_add(1);
                return ClientRenderApplicationBegin::DuplicateInProgress;
            }
            return ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::ApplicationFailure {
                    stage: RenderApplicationStage::Commit,
                },
                observed_state: RenderApplicationObservedState::NotApplicable,
            };
        }

        if self.applied_state.is_some()
            && (self.applied_connection_identity != Some(connection_identity)
                || self.applied_connection_generation != Some(identity.token.connection_generation))
            && identity.kind == RenderApplicationKind::Delta
        {
            return ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                observed_state: self.observation(),
            };
        }

        match identity.kind {
            RenderApplicationKind::Delta => {
                let Some(base_state) = identity.base_state else {
                    return ClientRenderApplicationBegin::Nack {
                        reason: RenderApplicationNackReason::MalformedOrIncomplete {
                            component: RenderApplicationComponent::Surface,
                        },
                        observed_state: RenderApplicationObservedState::NotApplicable,
                    };
                };
                match self.applied_state {
                    None => {
                        return ClientRenderApplicationBegin::Nack {
                            reason: RenderApplicationNackReason::BaseMismatch,
                            observed_state: RenderApplicationObservedState::Uninitialized,
                        };
                    }
                    Some(current) if current.render_generation != base_state.render_generation => {
                        return ClientRenderApplicationBegin::Nack {
                            reason: RenderApplicationNackReason::GenerationMismatch,
                            observed_state: RenderApplicationObservedState::Applied(current),
                        };
                    }
                    Some(current) if current.state_sequence < base_state.state_sequence => {
                        return ClientRenderApplicationBegin::Nack {
                            reason: RenderApplicationNackReason::DetectedGap,
                            observed_state: RenderApplicationObservedState::Applied(current),
                        };
                    }
                    Some(current) if current.state_sequence > base_state.state_sequence => {
                        return ClientRenderApplicationBegin::Nack {
                            reason: RenderApplicationNackReason::BaseMismatch,
                            observed_state: RenderApplicationObservedState::Applied(current),
                        };
                    }
                    Some(_) => {}
                }
            }
            RenderApplicationKind::Snapshot => {
                if self.applied_connection_identity == Some(connection_identity)
                    && self.applied_connection_generation
                        == Some(identity.token.connection_generation)
                {
                    if let Some(current) = self.applied_state {
                        if current.render_generation == identity.resulting_state.render_generation
                            && current.state_sequence >= identity.resulting_state.state_sequence
                        {
                            return ClientRenderApplicationBegin::Nack {
                                reason: RenderApplicationNackReason::BaseMismatch,
                                observed_state: RenderApplicationObservedState::Applied(current),
                            };
                        }
                    }
                }
            }
        }

        self.applying = Some(logical_identity);
        self.counters.applications_started = self.counters.applications_started.saturating_add(1);
        ClientRenderApplicationBegin::Apply
    }
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
struct ClientRenderApplicationGuard<'a> {
    state: &'a Mutex<ClientRenderApplicationState>,
    identity: RenderApplicationLogicalIdentity,
    armed: bool,
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
impl<'a> ClientRenderApplicationGuard<'a> {
    fn new(
        state: &'a Mutex<ClientRenderApplicationState>,
        connection_identity: RenderConnectionIdentity,
        identity: RenderApplicationIdentity,
    ) -> Self {
        Self {
            state,
            identity: RenderApplicationLogicalIdentity::new(connection_identity, identity),
            armed: true,
        }
    }

    fn acknowledge(mut self) {
        let mut state = self.state.lock();
        if state.applying == Some(self.identity)
            && state.active_connection_identity == Some(self.identity.connection_identity)
            && state.active_connection_generation == Some(self.identity.connection_generation)
        {
            state.applied_connection_generation = Some(self.identity.connection_generation);
            state.applied_connection_identity = Some(self.identity.connection_identity);
            state.applied_state = Some(self.identity.resulting_state);
            state.last_applied = Some(self.identity);
            state.applying = None;
            state.counters.acknowledgements = state.counters.acknowledgements.saturating_add(1);
        }
        self.armed = false;
    }

    fn nack(mut self) {
        let mut state = self.state.lock();
        if state.applying == Some(self.identity) {
            state.applying = None;
            state.counters.nacks = state.counters.nacks.saturating_add(1);
        }
        self.armed = false;
    }
}

impl Drop for ClientRenderApplicationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.state.lock();
        if state.applying == Some(self.identity) {
            state.applying = None;
            state.counters.cancelled_attempts = state.counters.cancelled_attempts.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "the live delivery integration must send settlements and explicitly coalesce in-progress duplicates"]
#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
pub(crate) enum ClientRenderApplicationDisposition {
    Settlement(RenderApplicationResult),
    DuplicateInProgress,
    ProtocolViolation(RenderApplicationContractError),
}

#[derive(Default)]
struct ClientSemanticState {
    zones: Vec<SemanticZone>,
    zone_texts: Vec<String>,
    last_exit_code: Option<i32>,
}

pub struct ClientPane {
    client: Arc<ClientInner>,
    local_pane_id: PaneId,
    pub remote_pane_id: PaneId,
    pub remote_tab_id: TabId,
    pub renderable: Arc<Mutex<RenderableState>>,
    configured_palette: Mutex<ColorPalette>,
    palette: Mutex<ColorPalette>,
    application_palette: Mutex<bool>,
    writer: Mutex<PaneWriter>,
    mouse: Arc<Mutex<MouseState>>,
    clipboard: Mutex<Option<Arc<dyn Clipboard>>>,
    mouse_grabbed: Mutex<bool>,
    alt_screen_active: Mutex<bool>,
    ignore_next_kill: Mutex<bool>,
    user_vars: Mutex<HashMap<String, String>>,
    config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    unseen_output: Mutex<bool>,
    progress: Mutex<Progress>,
    semantic_state: Mutex<ClientSemanticState>,
    #[allow(
        dead_code,
        reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
    )]
    render_application_state: Mutex<ClientRenderApplicationState>,
    #[allow(
        dead_code,
        reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
    )]
    render_application_limits: ClientRenderApplicationLimits,
    reliable_input_pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
    mux_registration: Arc<PaneRegistrationSlot>,
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
fn render_application_nack(
    connection_identity: RenderConnectionIdentity,
    identity: RenderApplicationIdentity,
    reason: RenderApplicationNackReason,
    observed_state: RenderApplicationObservedState,
) -> RenderApplicationResult {
    RenderApplicationResult {
        identity,
        outcome: RenderApplicationOutcome::Nack(RenderApplicationNack {
            reason,
            observed_state,
        }),
        connection_identity,
    }
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
fn render_application_ack(
    connection_identity: RenderConnectionIdentity,
    identity: RenderApplicationIdentity,
) -> RenderApplicationResult {
    RenderApplicationResult {
        identity,
        outcome: RenderApplicationOutcome::Applied {
            applied_state: identity.resulting_state,
        },
        connection_identity,
    }
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
fn bounded_resource_rejection(
    resource: RenderApplicationResource,
    requested: usize,
    limit: usize,
) -> RenderApplicationNackReason {
    RenderApplicationNackReason::BoundedResourceRejected {
        resource,
        requested: u64::try_from(requested).unwrap_or(u64::MAX),
        limit: u64::try_from(limit).unwrap_or(u64::MAX),
    }
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
fn validate_render_application_resources(
    update: &RenderApplicationUpdate,
    limits: ClientRenderApplicationLimits,
) -> Result<(), RenderApplicationNackReason> {
    if let Err(error) = update.validate() {
        return Err(match error {
            RenderApplicationContractError::ResourceLimitExceeded {
                resource,
                requested,
                limit,
            } => RenderApplicationNackReason::BoundedResourceRejected {
                resource,
                requested,
                limit,
            },
            RenderApplicationContractError::MalformedSurfaceComponent { component } => {
                RenderApplicationNackReason::MalformedOrIncomplete { component }
            }
            RenderApplicationContractError::TooManyAlerts => bounded_resource_rejection(
                RenderApplicationResource::Alerts,
                update.alerts.len(),
                MAX_RENDER_APPLICATION_ALERTS,
            ),
            RenderApplicationContractError::DuplicateStateAlert => {
                RenderApplicationNackReason::MalformedOrIncomplete {
                    component: RenderApplicationComponent::Alerts,
                }
            }
            _ => RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Surface,
            },
        });
    }

    if update.surface.dirty_lines.len() > limits.dirty_ranges {
        return Err(bounded_resource_rejection(
            RenderApplicationResource::Lines,
            update.surface.dirty_lines.len(),
            limits.dirty_ranges,
        ));
    }
    let mut prior_end = None;
    for range in &update.surface.dirty_lines {
        if range.is_empty() || prior_end.is_some_and(|end| end > range.start) {
            return Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Lines,
            });
        }
        prior_end = Some(range.end);
    }
    if update.surface.title.len() > limits.title_bytes {
        return Err(bounded_resource_rejection(
            RenderApplicationResource::Title,
            update.surface.title.len(),
            limits.title_bytes,
        ));
    }
    if let Some(working_dir) = &update.surface.working_dir {
        let requested = working_dir.as_str().len();
        if requested > limits.working_dir_bytes {
            return Err(bounded_resource_rejection(
                RenderApplicationResource::WorkingDirectory,
                requested,
                limits.working_dir_bytes,
            ));
        }
    }
    if update.surface.dimensions.scrollback_rows > limits.scrollback_rows {
        return Err(bounded_resource_rejection(
            RenderApplicationResource::Lines,
            update.surface.dimensions.scrollback_rows,
            limits.scrollback_rows,
        ));
    }
    let viewport_cells = update
        .surface
        .dimensions
        .cols
        .saturating_mul(update.surface.dimensions.viewport_rows);
    if viewport_cells > limits.viewport_cells {
        return Err(bounded_resource_rejection(
            RenderApplicationResource::Dimensions,
            viewport_cells,
            limits.viewport_cells,
        ));
    }

    let line_counts = update
        .surface
        .bonus_lines
        .validate_structure()
        .map_err(|error| {
            let component = match error {
                SerializedLinesStructureError::HyperlinkLineOutOfRange
                | SerializedLinesStructureError::HyperlinkCellRangeOutOfRange => {
                    RenderApplicationComponent::Hyperlinks
                }
                SerializedLinesStructureError::ImageLineMissing
                | SerializedLinesStructureError::ImageCellOutOfRange
                | SerializedLinesStructureError::ImageTextureCoordinatesInvalid => {
                    RenderApplicationComponent::Images
                }
                SerializedLinesStructureError::DuplicateStableRow
                | SerializedLinesStructureError::CellCountOverflow => {
                    RenderApplicationComponent::Lines
                }
            };
            RenderApplicationNackReason::MalformedOrIncomplete { component }
        })?;
    for (resource, requested, limit) in [
        (
            RenderApplicationResource::Lines,
            line_counts.lines,
            limits.lines,
        ),
        (
            RenderApplicationResource::Cells,
            line_counts.cells,
            limits.cells,
        ),
        (
            RenderApplicationResource::Hyperlinks,
            line_counts.hyperlink_spans,
            limits.hyperlink_spans,
        ),
        (
            RenderApplicationResource::Images,
            line_counts.images,
            limits.image_references,
        ),
    ] {
        if requested > limit {
            return Err(bounded_resource_rejection(resource, requested, limit));
        }
    }
    if line_counts.images > 0 && !limits.supports_images {
        return Err(RenderApplicationNackReason::UnsupportedResource {
            resource: RenderApplicationResource::Images,
        });
    }

    if let RenderComponentUpdate::Replace(semantic) = &update.semantic_zones {
        if !limits.supports_semantic_zones {
            return Err(RenderApplicationNackReason::UnsupportedResource {
                resource: RenderApplicationResource::SemanticZones,
            });
        }
        if semantic.zones.len() != semantic.zone_texts.len() {
            return Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::SemanticZones,
            });
        }
        if semantic.zones.len() > limits.semantic_zones {
            return Err(bounded_resource_rejection(
                RenderApplicationResource::SemanticZones,
                semantic.zones.len(),
                limits.semantic_zones,
            ));
        }
        let semantic_text_bytes = semantic.zone_texts.iter().try_fold(0usize, |total, text| {
            total.checked_add(text.len()).ok_or_else(|| {
                bounded_resource_rejection(
                    RenderApplicationResource::SemanticZones,
                    usize::MAX,
                    limits.semantic_text_bytes,
                )
            })
        })?;
        if semantic_text_bytes > limits.semantic_text_bytes {
            return Err(bounded_resource_rejection(
                RenderApplicationResource::SemanticZones,
                semantic_text_bytes,
                limits.semantic_text_bytes,
            ));
        }
        if semantic.zones.iter().any(|zone| {
            zone.start_y > zone.end_y || (zone.start_y == zone.end_y && zone.start_x > zone.end_x)
        }) {
            return Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::SemanticZones,
            });
        }
    }
    if matches!(&update.palette, RenderComponentUpdate::Replace(_)) && !limits.supports_palette {
        return Err(RenderApplicationNackReason::UnsupportedResource {
            resource: RenderApplicationResource::Palette,
        });
    }
    if !update.alerts.is_empty() && !limits.supports_alerts {
        return Err(RenderApplicationNackReason::UnsupportedResource {
            resource: RenderApplicationResource::Alerts,
        });
    }
    let alert_text_bytes = update.alert_text_bytes().unwrap_or(usize::MAX);
    if alert_text_bytes > limits.alert_text_bytes {
        return Err(bounded_resource_rejection(
            RenderApplicationResource::Alerts,
            alert_text_bytes,
            limits.alert_text_bytes,
        ));
    }

    Ok(())
}

impl ClientPane {
    pub(crate) fn new(
        client: &Arc<ClientInner>,
        local_pane_id: PaneId,
        remote_tab_id: TabId,
        remote_pane_id: PaneId,
        size: TerminalSize,
        title: &str,
        alt_screen_active: bool,
    ) -> Self {
        let mux_registration = Arc::new(PaneRegistrationSlot::default());
        let reliable_input_pane_authority = Arc::new(Mutex::new(None));
        let reliable_pane_write_delivery = ReliablePaneWriteDelivery::new();
        let writer = PaneWriter {
            client: Arc::clone(client),
            remote_pane_id,
            mux_registration: Arc::clone(&mux_registration),
            pane_authority: Arc::clone(&reliable_input_pane_authority),
            delivery: reliable_pane_write_delivery,
        };

        let mouse = Arc::new(Mutex::new(MouseState::new(
            remote_pane_id,
            client.client.clone(),
        )));

        let fetch_limiter =
            RateLimiter::new(|config| config.ratelimit_mux_line_prefetches_per_second);

        let renderable = Arc::new_cyclic(|weak_renderable| {
            Mutex::new(RenderableState {
                inner: RefCell::new(RenderableInner::new(
                    RenderablePaneBinding::new(
                        client,
                        remote_pane_id,
                        local_pane_id,
                        Arc::clone(&mux_registration),
                    ),
                    RenderableDimensions {
                        cols: size.cols as _,
                        viewport_rows: size.rows as _,
                        scrollback_rows: size.rows as _,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: size.dpi,
                        pixel_width: size.pixel_width,
                        pixel_height: size.pixel_height,
                        reverse_video: false,
                    },
                    title,
                    alt_screen_active,
                    fetch_limiter,
                    weak_renderable.clone(),
                )),
            })
        });

        let config = configuration();
        let palette: ColorPalette = config.resolved_palette.clone().into();

        Self {
            client: Arc::clone(client),
            mouse,
            remote_pane_id,
            local_pane_id,
            remote_tab_id,
            application_palette: Mutex::new(false),
            renderable,
            writer: Mutex::new(writer),
            configured_palette: Mutex::new(palette.clone()),
            palette: Mutex::new(palette),
            clipboard: Mutex::new(None),
            mouse_grabbed: Mutex::new(false),
            alt_screen_active: Mutex::new(alt_screen_active),
            ignore_next_kill: Mutex::new(false),
            unseen_output: Mutex::new(false),
            user_vars: Mutex::new(HashMap::new()),
            config: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            semantic_state: Mutex::new(ClientSemanticState::default()),
            render_application_state: Mutex::new(ClientRenderApplicationState::default()),
            render_application_limits: ClientRenderApplicationLimits::default(),
            reliable_input_pane_authority,
            mux_registration,
        }
    }

    pub(crate) fn prepare_render_application_bootstrap(
        &self,
        rpc: &RpcGenerationScope,
    ) -> anyhow::Result<bool> {
        if rpc.agreed_codec_version() == Some(LEGACY46_CODEC_VERSION) {
            // Codec 46 predates the authoritative render-application stream
            // and cannot supply its session identity. Its existing PDU24/25
            // pull renderer remains available; do not fabricate modern
            // authority merely to pass this bootstrap cut.
            return Ok(false);
        }
        let connection_generation = rpc
            .connection_generation()
            .ok_or_else(|| anyhow::anyhow!("render bootstrap has no exact RPC generation"))?
            .get();
        let connection_identity = rpc.render_connection_identity().ok_or_else(|| {
            anyhow::anyhow!("render bootstrap has no committed connection identity")
        })?;
        connection_identity.validate().map_err(anyhow::Error::new)?;
        Ok(self
            .render_application_state
            .lock()
            .prepare_authoritative_bootstrap(connection_identity, connection_generation))
    }

    /// Enqueue one sampled key-down on the same reliable lane as ordinary
    /// PDU96 input. The context is eligible only for the first effect-capable
    /// wire attempt; no-effect authority probes and later retries use PDU96.
    pub fn key_down_with_trace_context(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        trace_context: SampledTraceContextV1,
    ) -> anyhow::Result<()> {
        self.dispatch_key_down(key, mods, Some(trace_context))
    }

    /// Sampled counterpart to [`Pane::key_up`] with the same first-effect-
    /// capable-attempt rule as [`Self::key_down_with_trace_context`].
    pub fn key_up_with_trace_context(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        trace_context: SampledTraceContextV1,
    ) -> anyhow::Result<()> {
        self.dispatch_key_up(key, mods, Some(trace_context))
    }

    fn dispatch_key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        trace_context: Option<SampledTraceContextV1>,
    ) -> anyhow::Result<()> {
        let rpc = self.client.client.rpc_scope();
        let disposition = reliable_input_codec_disposition(rpc.agreed_codec_version());
        #[cfg(test)]
        self.client
            .reliable_input_queue
            .trigger_after_interactive_scope_capture_generation_barrier(&self.client.client);
        let input_serial = if disposition == ReliableInputCodecDisposition::Legacy {
            InputSerial::try_now()
                .ok_or_else(|| anyhow::anyhow!("process-local input serial space exhausted"))?
        } else {
            // The shared reliable queue assigns this identity while holding its
            // admission lock, linearizing key events with pane-byte range
            // reservations from other threads.
            InputSerial::empty()
        };
        let reliable_request = ReliableKeyEventV1 {
            pane_id: self.remote_pane_id,
            pane_registration: None,
            event: KeyEvent {
                key,
                modifiers: mods,
            },
            input_serial,
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        if let Some(trace_context) = trace_context {
            validate_sampled_keypress_trace_context(&trace_context)
                .context("validating sampled reliable key down")?;
        }
        if disposition == ReliableInputCodecDisposition::Legacy {
            let request = rpc.key_down(SendKeyDown {
                pane_id: reliable_request.pane_id,
                event: reliable_request.event,
                input_serial,
            });
            let renderable = self.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();
            dispatch_interactive_rpc(request, "key_down")?;
            inner.input_serial = input_serial;
            inner.predict_from_key_event(key, mods);
            inner.update_last_send();
            return Ok(());
        }
        let registration = self.mux_registration.load().ok_or_else(|| {
            anyhow::anyhow!("client pane is not bound to a live mux registration")
        })?;
        // Keep the same pane authority across bounded admission and prediction
        // publication. The shared reliable FIFO synchronously polls its first
        // RPC attempt in this callback, and a response handler needs this lock
        // before it can record the dispatch fence, so neither can overtake the
        // new prediction.
        let renderable = self.renderable.lock();
        let mut inner = renderable.inner.borrow_mut();
        let input_serial = self
            .client
            .reliable_input_queue
            .enqueue_with_trace_context(
                &self.client,
                registration,
                Arc::clone(&self.reliable_input_pane_authority),
                reliable_request,
                trace_context,
                Some(rpc),
            )?;
        inner.input_serial = input_serial;
        inner.predict_from_key_event(key, mods);
        inner.update_last_send();
        Ok(())
    }

    fn dispatch_key_up(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        trace_context: Option<SampledTraceContextV1>,
    ) -> anyhow::Result<()> {
        let rpc = self.client.client.rpc_scope();
        let disposition = reliable_input_codec_disposition(rpc.agreed_codec_version());
        #[cfg(test)]
        self.client
            .reliable_input_queue
            .trigger_after_interactive_scope_capture_generation_barrier(&self.client.client);
        let input_serial = if disposition == ReliableInputCodecDisposition::Legacy {
            InputSerial::try_now()
                .ok_or_else(|| anyhow::anyhow!("process-local input serial space exhausted"))?
        } else {
            InputSerial::empty()
        };
        let reliable_request = ReliableKeyEventV1 {
            pane_id: self.remote_pane_id,
            pane_registration: None,
            event: KeyEvent {
                key,
                modifiers: mods,
            },
            input_serial,
            kind: ReliableKeyEventKindV1::KeyUp,
        };
        if let Some(trace_context) = trace_context {
            validate_sampled_keypress_trace_context(&trace_context)
                .context("validating sampled reliable key up")?;
        }
        if disposition == ReliableInputCodecDisposition::Legacy {
            return dispatch_interactive_rpc(
                rpc.key_up(SendKeyUp {
                    pane_id: reliable_request.pane_id,
                    event: reliable_request.event,
                }),
                "key_up",
            );
        }
        let registration = self.mux_registration.load().ok_or_else(|| {
            anyhow::anyhow!("client pane is not bound to a live mux registration")
        })?;
        self.client
            .reliable_input_queue
            .enqueue_with_trace_context(
                &self.client,
                registration,
                Arc::clone(&self.reliable_input_pane_authority),
                reliable_request,
                trace_context,
                Some(rpc),
            )?;
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
    )]
    fn record_render_application_nack(&self) {
        let mut state = self.render_application_state.lock();
        state.counters.nacks = state.counters.nacks.saturating_add(1);
    }

    #[allow(
        dead_code,
        reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
    )]
    pub(crate) fn render_application_counters(&self) -> ClientRenderApplicationCounters {
        self.render_application_state.lock().counters
    }

    #[allow(
        dead_code,
        reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
    )]
    pub(crate) async fn apply_render_application(
        &self,
        registration: &mux::PaneRegistrationHandle,
        rpc: &RpcGenerationScope,
        mut update: RenderApplicationUpdate,
    ) -> ClientRenderApplicationDisposition {
        let connection_identity = update.connection_identity;
        let identity = update.identity;
        if let Err(error) = connection_identity
            .validate()
            .and_then(|()| identity.validate())
        {
            return ClientRenderApplicationDisposition::ProtocolViolation(error);
        }
        if let Err(reason) =
            validate_render_application_resources(&update, self.render_application_limits)
        {
            self.record_render_application_nack();
            return ClientRenderApplicationDisposition::Settlement(render_application_nack(
                connection_identity,
                identity,
                reason,
                RenderApplicationObservedState::NotApplicable,
            ));
        }
        let Some(application_deadline) = Instant::now().checked_add(Duration::from_millis(
            u64::from(update.retry_budget.remaining_millis),
        )) else {
            self.record_render_application_nack();
            return ClientRenderApplicationDisposition::Settlement(render_application_nack(
                connection_identity,
                identity,
                RenderApplicationNackReason::ApplicationFailure {
                    stage: RenderApplicationStage::Validate,
                },
                RenderApplicationObservedState::NotApplicable,
            ));
        };

        let expected_connection_generation =
            rpc.connection_generation().map(std::num::NonZeroU64::get);
        let expected_connection_identity = rpc.render_connection_identity();
        let begin = self.render_application_state.lock().begin(
            expected_connection_identity,
            expected_connection_generation,
            self.remote_pane_id,
            connection_identity,
            identity,
        );
        match begin {
            ClientRenderApplicationBegin::DuplicateApplied => {
                return ClientRenderApplicationDisposition::Settlement(render_application_ack(
                    connection_identity,
                    identity,
                ));
            }
            ClientRenderApplicationBegin::DuplicateInProgress => {
                return ClientRenderApplicationDisposition::DuplicateInProgress;
            }
            ClientRenderApplicationBegin::Nack {
                reason,
                observed_state,
            } => {
                self.record_render_application_nack();
                return ClientRenderApplicationDisposition::Settlement(render_application_nack(
                    connection_identity,
                    identity,
                    reason,
                    observed_state,
                ));
            }
            ClientRenderApplicationBegin::Apply => {}
        }

        let guard = ClientRenderApplicationGuard::new(
            &self.render_application_state,
            connection_identity,
            identity,
        );
        let bonus_lines = std::mem::take(&mut update.surface.bonus_lines);
        let bonus_lines = match hydrate_render_application_lines(
            rpc,
            update.surface.pane_id,
            bonus_lines,
            self.render_application_limits.image_bytes,
            application_deadline,
        )
        .await
        {
            Ok(lines) => lines,
            Err(reason) => {
                guard.nack();
                return ClientRenderApplicationDisposition::Settlement(render_application_nack(
                    connection_identity,
                    identity,
                    reason,
                    RenderApplicationObservedState::NotApplicable,
                ));
            }
        };

        let mouse_grabbed = update.surface.mouse_grabbed;
        let alt_screen_active = update.surface.alt_screen_active;
        let kind = identity.kind;
        let surface = update.surface;
        let semantic_zones = update.semantic_zones;
        let palette = update.palette;
        let alerts = update.alerts;
        if Instant::now() >= application_deadline {
            guard.nack();
            return ClientRenderApplicationDisposition::Settlement(render_application_nack(
                connection_identity,
                identity,
                RenderApplicationNackReason::ApplicationFailure {
                    stage: RenderApplicationStage::Commit,
                },
                RenderApplicationObservedState::NotApplicable,
            ));
        }
        let application = rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
            registration.try_with_current_output(|current| {
                if !current.is_same_pane_ref(self) {
                    return None;
                }
                let applied = self
                    .renderable
                    .lock()
                    .inner
                    .borrow_mut()
                    .apply_render_application_to_surface(surface, bonus_lines, kind);
                if !applied {
                    return Some(false);
                }
                *self.mouse_grabbed.lock() = mouse_grabbed;
                *self.alt_screen_active.lock() = alt_screen_active;

                if kind == RenderApplicationKind::Snapshot {
                    // Snapshot alert absence is authoritative for latest-value
                    // state. Deltas leave these values untouched unless they
                    // carry the corresponding coalesced alert below.
                    *self.unseen_output.lock() = false;
                    *self.progress.lock() = Progress::None;
                }
                if let RenderComponentUpdate::Replace(semantic) = semantic_zones {
                    *self.semantic_state.lock() = ClientSemanticState {
                        zones: semantic.zones,
                        zone_texts: semantic.zone_texts,
                        last_exit_code: semantic.last_exit_code,
                    };
                }
                if let RenderComponentUpdate::Replace(palette) = palette {
                    let palette = Arc::unwrap_or_clone(palette.palette);
                    *self.application_palette.lock() = palette != *self.configured_palette.lock();
                    *self.palette.lock() = palette;
                    current.dispatch_alert(Alert::PaletteChanged);
                }
                for NotifyAlert { alert, .. } in alerts {
                    match &alert {
                        Alert::SetUserVar { name, value } => {
                            self.user_vars.lock().insert(name.clone(), value.clone());
                        }
                        Alert::OutputSinceFocusLost => {
                            *self.unseen_output.lock() = true;
                        }
                        Alert::Progress(progress) => {
                            *self.progress.lock() = progress.clone();
                        }
                        _ => {}
                    }
                    current.dispatch_alert(alert);
                }
                Some(true)
            })
        });

        let failure_stage = match application {
            Ok(Some(Some(true))) => {
                guard.acknowledge();
                return ClientRenderApplicationDisposition::Settlement(render_application_ack(
                    connection_identity,
                    identity,
                ));
            }
            Ok(Some(Some(false))) => RenderApplicationStage::ApplySurface,
            Ok(Some(None)) | Ok(None) | Err(_) => RenderApplicationStage::Commit,
        };
        guard.nack();
        ClientRenderApplicationDisposition::Settlement(render_application_nack(
            connection_identity,
            identity,
            RenderApplicationNackReason::ApplicationFailure {
                stage: failure_stage,
            },
            RenderApplicationObservedState::NotApplicable,
        ))
    }

    pub(crate) async fn process_unilateral(
        &self,
        registration: &mux::PaneRegistrationHandle,
        rpc: &RpcGenerationScope,
        pdu: Pdu,
    ) -> anyhow::Result<()> {
        let registration_matches_self = registration
            .try_with_current(|current| current.is_same_pane_ref(self))
            .unwrap_or(false);
        if !registration_matches_self {
            log::trace!(
                "discarding unilateral PDU for mismatched or stale client pane registration {}",
                self.local_pane_id
            );
            return Ok(());
        }

        match pdu {
            Pdu::GetPaneRenderChangesResponse(mut delta) => {
                if delta.pane_id != self.remote_pane_id {
                    bail!(
                        "unilateral render response pane mismatch: expected {}, got {}",
                        self.remote_pane_id,
                        delta.pane_id
                    );
                }
                let mouse_grabbed = delta.mouse_grabbed;
                let alt_screen_active = delta.alt_screen_active;
                let current_seqno = registration.try_with_current(|_| {
                    let renderable = self.renderable.lock();
                    renderable.get_current_seqno()
                });
                let Some(current_seqno) = current_seqno else {
                    return Ok(());
                };
                if !should_process_unilateral_render_delta(
                    current_seqno,
                    delta.seqno,
                    delta.input_serial,
                ) {
                    return Ok(());
                }
                let stale_dispatch_ack = current_seqno > delta.seqno;

                let serialized_bonus_lines = std::mem::take(&mut delta.bonus_lines);
                let (bonus_lines, incomplete_image_rows) = if stale_dispatch_ack {
                    // The surface content is stale and will be rejected below. Do
                    // not spend decompression or image-hydration work on it; the
                    // dispatch serial and sequence fence are the only admissible
                    // information in this reordered response.
                    (Vec::new(), Default::default())
                } else {
                    hydrate_lines(rpc, delta.pane_id, serialized_bonus_lines)
                        .await?
                        .into_parts()
                };

                let applied = rpc
                    .commit_sync(RpcConsumerKind::PaneUnilateral, || {
                        registration
                            .try_with_current_output(|_| {
                                let applied = {
                                    let renderable = self.renderable.lock();
                                    let mut inner = renderable.inner.borrow_mut();
                                    let applied =
                                        inner.apply_changes_to_surface(delta, bonus_lines);
                                    if applied {
                                        inner.mark_image_hydration_incomplete_rows(
                                            &incomplete_image_rows,
                                        );
                                    }
                                    applied
                                };
                                if applied {
                                    *self.mouse_grabbed.lock() = mouse_grabbed;
                                    *self.alt_screen_active.lock() = alt_screen_active;
                                }
                                applied
                            })
                            .unwrap_or(false)
                    })
                    .map_err(anyhow::Error::new)?;
                if !applied {
                    if stale_dispatch_ack {
                        log::trace!(
                            "recorded reordered input dispatch fence without applying stale surface content for client pane {}",
                            self.local_pane_id
                        );
                    } else {
                        log::trace!(
                            "discarding render delta for stale client pane registration {}",
                            self.local_pane_id
                        );
                    }
                }
            }
            Pdu::SetClipboard(SetClipboard {
                clipboard,
                selection,
                ..
            }) => {
                let result = rpc
                    .commit_sync(RpcConsumerKind::PaneUnilateral, || {
                        registration.try_with_current(|current| {
                            if !current.is_same_pane_ref(self) {
                                return Ok(());
                            }
                            let clipboard_handler = { self.clipboard.lock().clone() };
                            match clipboard_handler {
                                Some(clip) => {
                                    log::debug!(
                                        "Pdu::SetClipboard pane={} remote={} {:?} {:?}",
                                        self.local_pane_id,
                                        self.remote_pane_id,
                                        selection,
                                        clipboard
                                    );
                                    clip.set_contents(selection, clipboard)
                                }
                                None => {
                                    log::error!(
                                        "ClientPane: Ignoring SetClipboard request {:?}",
                                        clipboard
                                    );
                                    Ok(())
                                }
                            }
                        })
                    })
                    .map_err(anyhow::Error::new)?;
                if let Some(result) = result {
                    result?;
                }
            }
            Pdu::SetPalette(SetPalette { palette, .. }) => {
                rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
                    let _ = registration.try_with_current(|current| {
                        let palette = Arc::unwrap_or_clone(palette);
                        *self.application_palette.lock() =
                            palette != *self.configured_palette.lock();
                        *self.palette.lock() = palette;
                        self.renderable.lock().inner.borrow_mut().make_all_stale();
                        current.dispatch_alert(Alert::PaletteChanged);
                    });
                })
                .map_err(anyhow::Error::new)?;
            }
            Pdu::NotifyAlert(NotifyAlert { alert, .. }) => {
                rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
                    let _ = registration.try_with_current(|current| {
                        match &alert {
                            Alert::SetUserVar { name, value } => {
                                self.user_vars.lock().insert(name.clone(), value.clone());
                            }
                            Alert::OutputSinceFocusLost => {
                                *self.unseen_output.lock() = true;
                            }
                            Alert::Progress(progress) => {
                                *self.progress.lock() = progress.clone();
                            }
                            _ => {}
                        }
                        current.dispatch_alert(alert);
                    });
                })
                .map_err(anyhow::Error::new)?;
            }
            Pdu::PaneRemoved(PaneRemoved { pane_id }) => {
                log::trace!("remote pane {} has been removed", pane_id);
                rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
                    let _ = registration.try_with_current(|current| {
                        self.renderable.lock().inner.borrow_mut().dead = true;
                        current.prune_dead_windows();
                        self.client.expire_stale_mappings(&current);
                    });
                })
                .map_err(anyhow::Error::new)?;
            }
            Pdu::PaneFocused(PaneFocused { pane_id }) => {
                // We get here whenever the pane focus is changed on the
                // server. That might be due to the user here in the GUI
                // doing things, or it may be due to a "remote"
                // `wezterm cli activate-pane-direction` or similar call
                // from some other actor.
                // The latter case is the important one: it is desirable
                // for the focus change to be reflected locally after it
                // has been changed on the server, so we work to apply
                // it here.
                log::trace!("advised of remote pane focus: {pane_id}");

                rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
                    let _ = registration.try_with_current(|current| {
                        if let Err(err) = current.focus_pane_and_containing_tab() {
                            log::error!(
                                "Error reconciling remote PaneFocused notification: {err:#}"
                            );
                        }
                    });
                })
                .map_err(anyhow::Error::new)?;
            }
            _ => bail!("unhandled unilateral pdu: {:?}", pdu),
        };
        Ok(())
    }

    pub fn remote_pane_id(&self) -> PaneId {
        self.remote_pane_id
    }

    /// Send one sampled paste without placing its content in trace metadata.
    /// Peers below the additive traced-paste dialect receive the unchanged
    /// PDU13 request; the context is consumed rather than retained across a
    /// later connection generation.
    pub fn send_paste_with_trace_context(
        &self,
        text: &str,
        trace_context: SampledTraceContextV1,
    ) -> anyhow::Result<()> {
        self.dispatch_paste(text, Some(trace_context))
    }

    fn dispatch_paste(
        &self,
        text: &str,
        trace_context: Option<SampledTraceContextV1>,
    ) -> anyhow::Result<()> {
        if let Some(trace_context) = trace_context {
            validate_sampled_paste_trace_context(&trace_context)
                .context("validating sampled paste before input-serial allocation")?;
            if text.len() > MAX_SEND_PASTE_TRACED_V1_DECOMPRESSED_BYTES {
                bail!(
                    "sampled paste payload exceeds the fixed {}-byte decoded ceiling",
                    MAX_SEND_PASTE_TRACED_V1_DECOMPRESSED_BYTES
                );
            }
        }
        let input_serial = InputSerial::try_now()
            .ok_or_else(|| anyhow::anyhow!("process-local input serial space exhausted"))?;
        let client = Arc::clone(&self.client);
        let rpc = client.client.rpc_scope();
        let agreed_codec_version = rpc.agreed_codec_version();
        #[cfg(test)]
        client
            .reliable_input_queue
            .trigger_after_interactive_scope_capture_generation_barrier(&client.client);
        let request = SendPaste {
            pane_id: self.remote_pane_id,
            data: text.to_owned(),
            input_serial,
        };
        let renderable = self.renderable.lock();
        let mut inner = renderable.inner.borrow_mut();
        match trace_context {
            Some(trace_context)
                if agreed_codec_version
                    .is_some_and(|version| version >= SEND_PASTE_TRACED_V1_MIN_CODEC_VERSION) =>
            {
                dispatch_interactive_rpc(
                    rpc.send_paste_traced_v1(SendPasteTracedV1 {
                        request,
                        trace_context,
                    }),
                    "send_paste_traced_v1",
                )?;
            }
            _ => dispatch_interactive_rpc(rpc.send_paste(request), "send_paste")?,
        }
        inner.input_serial = input_serial;
        inner.predict_from_paste(text);
        inner.update_last_send();
        Ok(())
    }

    pub(crate) fn belongs_to_client(&self, client: &ClientInner) -> bool {
        std::ptr::eq(self.client.as_ref(), client)
    }

    /// Arrange to suppress the next Pane::kill call.
    /// This is a bit of a hack that we use when closing a window;
    /// our Domain::local_window_is_closing impl calls this for each
    /// ClientPane in the window so that closing a window effectively
    /// "detaches" the window so that reconnecting later will resume
    /// from where they left off.
    /// It isn't perfect.
    pub fn ignore_next_kill(&self) {
        *self.ignore_next_kill.lock() = true;
    }

    pub fn sync_remote_listing_state(&self, alt_screen_active: bool) {
        *self.alt_screen_active.lock() = alt_screen_active;
    }
}

#[async_trait(?Send)]
impl Pane for ClientPane {
    fn pane_id(&self) -> PaneId {
        self.local_pane_id
    }

    fn mux_registration_slot(&self) -> &Arc<PaneRegistrationSlot> {
        &self.mux_registration
    }

    fn mux_registration_did_bind(&self, registration: mux::PaneRegistrationHandle) {
        let registration_matches_self = registration
            .try_with_current(|current| current.is_same_pane_ref(self))
            .unwrap_or(false);
        if !registration_matches_self {
            log::trace!(
                "skipping client pane bind work for mismatched or stale registration {}",
                self.local_pane_id
            );
            return;
        }

        self.renderable
            .lock()
            .inner
            .borrow_mut()
            .registration_did_bind();

        // Advise the server only after the pane has acquired exact mux
        // registration authority. Re-check the pane-owned slot when the
        // detached task runs so work beginning after retirement/rebind is
        // discarded.
        let mux_registration = Arc::clone(&self.mux_registration);
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let palette = self.configured_palette.lock().clone();
        let request = SetPalette {
            pane_id: remote_pane_id,
            palette: Arc::new(palette),
        };
        let rpc = client.client.rpc_scope();
        match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Topology,
            4 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                reservation
                    .spawn(async move {
                        let registration_is_current = mux_registration
                            .load()
                            .is_some_and(|current| current.same_registration(&registration))
                            && registration.try_with_current(|_| ()).is_some();
                        if !registration_is_current {
                            return Ok(());
                        }

                        rpc.set_configured_palette_for_pane(request)
                            .await
                            .map(|_| ())
                    })
                    .detach();
            }
            rejected => {
                let abort = client.client.abort_rpc_transport_generation(
                    &rpc,
                    "pane-bind palette scheduler admission failed",
                );
                log::error!(
                    "main-thread scheduler rejected pane-bind palette convergence; aborted exact RPC generation ({abort:?}): {rejected:?}"
                );
            }
        }
    }

    fn get_metadata(&self) -> Value {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();

        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        map.insert(
            Value::String("is_tardy".to_string()),
            Value::Bool(inner.is_tardy()),
        );
        map.insert(
            Value::String("since_last_response_ms".to_string()),
            Value::U64(
                u64::try_from(inner.last_recv_time.elapsed().as_millis()).unwrap_or(u64::MAX),
            ),
        );

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        self.renderable.lock().get_cursor_position()
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.renderable.lock().get_dimensions()
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
    }

    fn with_lines_mut_and_apply_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[termwiz::hyperlink::Rule],
        with_lines: &mut dyn WithPaneLines,
    ) {
        // Rule selection and extraction share one authoritative cache snapshot.
        // The callback cannot run under the renderable lock because pane
        // consumers may re-enter pane APIs. Afterward, write back only appdata
        // from projections whose content still exactly matches the cache; this
        // preserves remote shape hashes without admitting prediction/overlay
        // metadata or a stale completion.
        let renderable = self.renderable.lock();
        let (first, mut owned_lines) = renderable.get_lines_with_hyperlinks(lines, rules);
        drop(renderable);
        let mut line_refs = owned_lines.iter_mut().collect::<Vec<_>>();
        with_lines.with_lines_mut(first, &mut line_refs);
        drop(line_refs);
        self.renderable
            .lock()
            .write_back_unchanged_line_appdata(first, &owned_lines);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        self.renderable.lock().get_lines(lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.renderable.lock().get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        self.renderable.lock().get_changed_since(lines, seqno)
    }

    fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        self.renderable
            .lock()
            .get_changed_since_with_source_fence(lines, last_observed_source_end)
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.clipboard.lock().replace(Arc::clone(clipboard));
    }

    fn get_title(&self) -> String {
        let renderable = self.renderable.lock();
        let inner = renderable.inner.borrow();
        inner.title.clone()
    }

    fn get_progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        self.dispatch_paste(text, None)
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn set_zoomed(&self, zoomed: bool) {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let remote_tab_id = self.remote_tab_id;
        // Invalidate any cached rows on a resize
        inner.make_all_stale();
        let request = client.client.set_zoomed(SetPaneZoomed {
            containing_tab_id: remote_tab_id,
            pane_id: remote_pane_id,
            zoomed,
        });
        if let Err(error) = dispatch_interactive_rpc(request, "set pane zoom") {
            log::error!("failed to schedule pane zoom convergence: {error:#}");
        }
        inner.update_last_send();
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        let render = self.renderable.lock();
        let mut inner = render.inner.borrow_mut();

        let cols = size.cols;
        let rows = size.rows;

        if inner.dimensions.cols != cols
            || inner.dimensions.viewport_rows != rows
            || inner.dimensions.pixel_width != size.pixel_width
            || inner.dimensions.pixel_height != size.pixel_height
        {
            inner.dimensions.cols = cols;
            inner.dimensions.viewport_rows = rows;
            inner.dimensions.pixel_width = size.pixel_width;
            inner.dimensions.pixel_height = size.pixel_height;

            // Invalidate any cached rows on a resize
            inner.make_all_stale();

            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let remote_tab_id = self.remote_tab_id;
            let request = client.client.resize(Resize {
                containing_tab_id: remote_tab_id,
                pane_id: remote_pane_id,
                size,
            });
            dispatch_interactive_rpc(request, "resize pane")?;
            inner.update_last_send();
        }
        Ok(())
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let rpc = self.client.client.rpc_scope();
        match rpc
            .search_scrollback(SearchScrollbackRequest {
                pane_id: self.remote_pane_id,
                pattern,
                range,
                limit,
            })
            .await
        {
            Ok(SearchScrollbackResponse { results }) => rpc
                .commit_sync(RpcConsumerKind::Search, || results)
                .map_err(anyhow::Error::new),
            Err(e) => Err(e),
        }
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        self.dispatch_key_down(key, mods, None)
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        self.dispatch_key_up(key, mods, None)
    }

    fn kill(&self) {
        let mut ignore = self.ignore_next_kill.lock();
        if *ignore {
            *ignore = false;
            return;
        }
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;

        // We only want to ask the server to kill the pane if the user
        // explicitly requested it to die.
        // Domain detaching can implicitly call Pane::kill on the panes
        // in the domain, so we need to check here whether the domain is
        // in the detached state; if so then we must skip sending the
        // kill to the server.
        if !client.is_detached() {
            let request = client.client.kill_pane(KillPane {
                pane_id: remote_pane_id,
            });
            if let Err(error) = dispatch_interactive_rpc(request, "kill pane") {
                log::error!("failed to schedule explicit remote pane kill: {error:#}");
            }
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.mouse.lock().append(event);
        if MouseState::next(Arc::clone(&self.mouse)) {
            self.renderable.lock().inner.borrow_mut().update_last_send();
        }
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.renderable.lock().inner.borrow().dead
    }

    fn palette(&self) -> ColorPalette {
        self.palette.lock().clone()
    }

    fn domain_id(&self) -> DomainId {
        self.client.local_domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        *self.mouse_grabbed.lock()
    }

    fn is_alt_screen_active(&self) -> bool {
        *self.alt_screen_active.lock()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.renderable.lock().inner.borrow().working_dir.clone()
    }

    fn focus_changed(&self, focused: bool) {
        if focused {
            self.advise_focus();
            *self.unseen_output.lock() = false;
        }
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.erase_scrollback(EraseScrollbackRequest {
            pane_id: remote_pane_id,
            erase_mode,
        });
        if let Err(error) = dispatch_interactive_rpc(request, "erase scrollback") {
            log::error!("failed to schedule remote scrollback erase: {error:#}");
        }
    }

    fn advise_focus(&self) {
        let mut focused_pane = lock_or_recover(
            &self.client.focused_remote_pane_id,
            "focused_remote_pane_id",
        );
        if *focused_pane != Some(self.remote_pane_id) {
            focused_pane.replace(self.remote_pane_id);
            let client = Arc::clone(&self.client);
            let remote_pane_id = self.remote_pane_id;
            let request = client.client.set_focused_pane_id(SetFocusedPane {
                pane_id: remote_pane_id,
            });
            if let Err(error) = dispatch_interactive_rpc(request, "set focused pane") {
                log::error!("failed to schedule remote focused-pane convergence: {error:#}");
            }
        }
    }

    fn has_unseen_output(&self) -> bool {
        *self.unseen_output.lock()
    }

    fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        match reason {
            CloseReason::Window => true,
            CloseReason::Tab => false,
            CloseReason::Pane => false,
        }
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.user_vars.lock().clone()
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        Ok(self.semantic_state.lock().zones.clone())
    }

    fn get_text_from_semantic_zone(&self, zone: SemanticZone) -> anyhow::Result<String> {
        let semantic = self.semantic_state.lock();
        if let Some(index) = semantic
            .zones
            .iter()
            .position(|candidate| *candidate == zone)
        {
            return Ok(semantic.zone_texts[index].clone());
        }
        drop(semantic);
        Ok(mux::pane::text_from_semantic_zone(self, zone))
    }

    fn get_semantic_exit_code(&self) -> anyhow::Result<Option<i32>> {
        Ok(self.semantic_state.lock().last_exit_code)
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        let palette = config.color_palette();
        // If the application running in the pane hasn't changed the
        // palette through escape sequences, speculatively adopt the
        // new palette so that it updates with the lowest latency.
        if !*self.application_palette.lock() {
            *self.palette.lock() = palette.clone();
        }
        *self.configured_palette.lock() = palette.clone();

        // and now send the color palette to the server
        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let request = client.client.set_configured_palette_for_pane(SetPalette {
            pane_id: remote_pane_id,
            palette: Arc::new(palette),
        });
        if let Err(error) = dispatch_interactive_rpc(request, "set configured pane palette") {
            log::error!("failed to schedule configured palette convergence: {error:#}");
        }
        self.config.lock().replace(config);
        // Implicit hyperlink rules are selected by each GUI window when it
        // borrows lines for rendering. Do not walk the complete remote line
        // cache here: one pane can be projected through windows with different
        // rule sets, and the next projection performs the exact lazy epoch
        // transition under the renderable lock.
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        self.config.lock().clone()
    }
}

struct PaneWriter {
    client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    mux_registration: Arc<PaneRegistrationSlot>,
    pane_authority: Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
    delivery: Arc<ReliablePaneWriteDelivery>,
}

impl std::io::Write for PaneWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        if let Some(failure) = self.delivery.sticky_failure() {
            return Err(std::io::Error::new(failure.io_kind(), failure.message()));
        }
        let registration = self.mux_registration.load().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "client pane has no live mux registration",
            )
        })?;
        // `Ok(n)` is an ownership acknowledgement, not a remote-delivery ACK:
        // the shared bounded FIFO now owns exactly the first `n` bytes and will
        // either settle their exact applied prefixes or publish a sticky,
        // user-visible terminal failure. Codec-46 uses one transport-fenced
        // legacy PDU9 attempt; capable peers use their durable replay ledger.
        // PDU construction begins only after the GUI callback yields.
        self.client.reliable_input_queue.enqueue_pane_write(
            &self.client,
            registration,
            self.remote_pane_id,
            Arc::clone(&self.pane_authority),
            Arc::clone(&self.delivery),
            data,
        )
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        let pending = self
            .delivery
            .flush_pending()
            .map_err(|failure| std::io::Error::new(failure.io_kind(), failure.message()))?;
        if !pending {
            return Ok(());
        }
        let on_mux_main_thread = self
            .mux_registration
            .load()
            .and_then(|registration| {
                registration.try_with_current(|current| current.owner_is_main_thread())
            })
            .unwrap_or(true);
        if on_mux_main_thread {
            // The FIFO worker itself is serviced by this scheduler. Blocking
            // here would recreate the GUI deadlock that this path replaces.
            // Pending ownership remains observable on every flush until the
            // worker settles it.
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "reliable pane input is still pending on the mux main-thread scheduler",
            ));
        }
        if let Some(failure) = self.delivery.wait_until_settled() {
            return Err(std::io::Error::new(failure.io_kind(), failure.message()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Client, TestRpcPeer, TEST_RENDER_CONNECTION_IDENTITY};
    use crate::domain::ClientDomainConfig;
    use crate::MuxTestScope;
    use config::UnixDomain;
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use mux::{Mux, MuxNotification};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;
    use termwiz::cell::{CellAttributes, SemanticType};

    const SUCCESSOR_RENDER_CONNECTION_IDENTITY: RenderConnectionIdentity =
        RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x79; 16]),
            TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
        );
    const ROUTE_FAILOVER_RENDER_CONNECTION_IDENTITY: RenderConnectionIdentity =
        RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x7a; 16]),
            TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
        );
    const SERVER_RESTART_RENDER_CONNECTION_IDENTITY: RenderConnectionIdentity =
        RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x7a; 16]),
            MuxSessionIncarnation::from_bytes([0x9c; 16]),
        );

    fn pane_authority_binding(
        session_incarnation: MuxSessionIncarnation,
        pane_registration: ReliablePaneRegistrationIdentityV1,
    ) -> ReliablePaneInputAuthority {
        ReliablePaneInputAuthority {
            session_incarnation,
            pane_registration,
        }
    }

    fn test_pane_authority(
        pane_registration: ReliablePaneRegistrationIdentityV1,
    ) -> Arc<Mutex<Option<ReliablePaneInputAuthority>>> {
        Arc::new(Mutex::new(Some(pane_authority_binding(
            TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            pane_registration,
        ))))
    }

    fn cached_pane_registration(
        pane_authority: &Arc<Mutex<Option<ReliablePaneInputAuthority>>>,
    ) -> Option<ReliablePaneRegistrationIdentityV1> {
        pane_authority
            .lock()
            .as_ref()
            .map(|authority| authority.pane_registration)
    }

    fn sampled_reliable_key_context() -> SampledTraceContextV1 {
        SampledTraceContextV1 {
            schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
            trace_id: InteractionTraceId {
                run_id: InteractionTraceRunId {
                    epoch_nonce_hi: 0x1112_1314_1516_1718,
                    epoch_nonce_lo: 0x2122_2324_2526_2728,
                },
                sequence: 29,
            },
            path: InteractionTracePath::Keypress,
            origin_recorder_epoch_id: RecorderEpochId {
                nonce_hi: 0x3132_3334_3536_3738,
                nonce_lo: 0x4142_4344_4546_4748,
            },
            sampler_algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
        }
    }

    fn sampled_paste_context() -> SampledTraceContextV1 {
        SampledTraceContextV1 {
            path: InteractionTracePath::Paste,
            ..sampled_reliable_key_context()
        }
    }

    #[test]
    fn reordered_input_dispatch_ack_survives_unilateral_stale_filter() {
        let serial = InputSerial::now();

        assert!(!should_process_unilateral_render_delta(11, 10, None));
        assert!(should_process_unilateral_render_delta(11, 10, Some(serial)));
        assert!(should_process_unilateral_render_delta(11, 11, None));
        assert!(should_process_unilateral_render_delta(11, 12, None));
    }

    #[test]
    fn reliable_input_retry_backoff_is_bounded_and_keeps_the_first_retry_prompt() {
        let base = Duration::from_millis(1);
        assert_eq!(reliable_input_retry_delay(base, 0), base);
        assert_eq!(
            reliable_input_retry_delay(base, 1),
            Duration::from_millis(2)
        );
        assert_eq!(
            reliable_input_retry_delay(base, 6),
            Duration::from_millis(64)
        );
        assert_eq!(
            reliable_input_retry_delay(base, u32::MAX),
            RELIABLE_INPUT_MAX_RETRY_DELAY
        );
        let authoritative = Duration::from_secs(1);
        assert_eq!(
            reliable_input_retry_delay(authoritative, u32::MAX),
            authoritative,
            "local exponential backoff must not shorten the peer's retry authority"
        );
    }

    #[test]
    fn reliable_input_codec_boundary_never_downgrades_a_queued_retry() {
        assert_eq!(
            reliable_input_codec_disposition(None),
            ReliableInputCodecDisposition::AwaitingAuthority
        );
        assert_eq!(
            reliable_input_codec_disposition(Some(
                RELIABLE_KEY_EVENT_V1_MIN_CODEC_VERSION.saturating_sub(1)
            )),
            ReliableInputCodecDisposition::Legacy
        );
        assert_eq!(
            reliable_input_codec_disposition(Some(RELIABLE_KEY_EVENT_V1_MIN_CODEC_VERSION)),
            ReliableInputCodecDisposition::Reliable
        );
        assert_eq!(
            reliable_input_codec_disposition(Some(RELIABLE_KEY_EVENT_V1_MIN_CODEC_VERSION + 1)),
            ReliableInputCodecDisposition::Reliable
        );
        assert_eq!(
            reliable_input_codec_disposition(Some(RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION)),
            ReliableInputCodecDisposition::ReliableTraced
        );
    }

    #[test]
    fn reliable_input_trace_context_waits_for_authority_then_is_consumed_once() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 46, 32);
        mux.add_pane(&pane)
            .expect("register traced input test pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture exact traced input pane registration");
        {
            let mut state = inner.reliable_input_queue.state.lock();
            state.worker_running = true;
        }
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x71; 16]);
        let pane_authority = Arc::new(Mutex::new(None));
        let request = ReliableKeyEventV1 {
            pane_id: 32,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('t'),
                modifiers: KeyModifiers::CTRL,
            },
            input_serial: InputSerial::from_millis_since_epoch(41),
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        let context = sampled_reliable_key_context();
        inner
            .reliable_input_queue
            .enqueue_with_trace_context(
                &inner,
                registration,
                pane_authority,
                request.clone(),
                Some(context),
                None,
            )
            .expect("valid sampled reliable key must enter the existing FIFO");
        let expected = inner
            .reliable_input_queue
            .state
            .lock()
            .pending
            .front()
            .expect("queued input exists")
            .clone();

        let (probe, consumed) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("no-effect authority probe must remain operationally reliable");
        assert!(consumed.is_none());
        let ReliableInputWireAttempt::Unsampled(probe) = probe else {
            panic!("authority probe must use byte-identical PDU96");
        };
        assert!(probe.pane_registration.is_none());
        assert_eq!(
            inner
                .reliable_input_queue
                .state
                .lock()
                .pending
                .front()
                .and_then(QueuedReliableInput::key_trace_context),
            Some(context),
            "authority probe must retain context for the first effect-eligible attempt"
        );
        assert!(inner.reliable_input_queue.bind_front_pane_authority(
            &expected,
            pane_authority_binding(
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
                pane_registration,
            ),
        ));

        let (first, consumed) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("first authority-bound attempt must claim its trace context");
        assert_eq!(consumed, Some(context));
        let ReliableInputWireAttempt::Traced(first) = first else {
            panic!("v62 first effect-eligible attempt must use the traced wrapper");
        };
        assert_eq!(first.request.pane_registration, Some(pane_registration));
        assert_eq!(first.trace_context, context);

        let (retry, consumed) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("same operational request remains retryable");
        assert!(consumed.is_none());
        assert!(matches!(retry, ReliableInputWireAttempt::Unsampled(_)));
        assert_eq!(
            inner
                .reliable_input_queue
                .state
                .lock()
                .pending
                .front()
                .expect("retry remains queued")
                .key_request()
                .expect("retry remains a key input"),
            &request
        );
    }

    #[test]
    fn v61_effect_eligible_attempt_consumes_context_before_a_v62_retry() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 47, 33);
        mux.add_pane(&pane)
            .expect("register trace restoration pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture trace restoration registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let request = ReliableKeyEventV1 {
            pane_id: 33,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
            },
            input_serial: InputSerial::from_millis_since_epoch(42),
            kind: ReliableKeyEventKindV1::KeyUp,
        };
        let context = sampled_reliable_key_context();
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x72; 16]);
        inner
            .reliable_input_queue
            .enqueue_with_trace_context(
                &inner,
                registration,
                test_pane_authority(pane_registration),
                request,
                Some(context),
                None,
            )
            .expect("sampled input should queue");
        let expected = inner.reliable_input_queue.state.lock().pending[0].clone();

        let (v61_attempt, consumed) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::Reliable,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("v61 attempt should retain reliable operational semantics");
        assert!(matches!(
            v61_attempt,
            ReliableInputWireAttempt::Unsampled(_)
        ));
        assert_eq!(consumed, Some(context));

        let (v62_attempt, consumed_again) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("an ambiguous v61 attempt remains retryable after an upgrade");
        assert!(matches!(
            v62_attempt,
            ReliableInputWireAttempt::Unsampled(_)
        ));
        assert!(consumed_again.is_none());
    }

    #[test]
    fn definitely_not_sent_effect_eligible_attempt_may_restore_trace_context() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 48, 34);
        mux.add_pane(&pane)
            .expect("register trace restoration pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture trace restoration registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x73; 16]);
        let request = ReliableKeyEventV1 {
            pane_id: 34,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('s'),
                modifiers: KeyModifiers::SHIFT,
            },
            input_serial: InputSerial::from_millis_since_epoch(43),
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        let context = sampled_reliable_key_context();
        inner
            .reliable_input_queue
            .enqueue_with_trace_context(
                &inner,
                registration,
                test_pane_authority(pane_registration),
                request,
                Some(context),
                None,
            )
            .expect("sampled input should queue");
        let expected = inner.reliable_input_queue.state.lock().pending[0].clone();

        let (v61_attempt, consumed) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::Reliable,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("v61 attempt should retain reliable operational semantics");
        assert!(matches!(
            v61_attempt,
            ReliableInputWireAttempt::Unsampled(_)
        ));
        assert_eq!(consumed, Some(context));
        assert!(inner
            .reliable_input_queue
            .restore_front_trace_context(&expected, consumed));

        let (v62_attempt, consumed_again) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &expected,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("definitely-not-sent restoration preserves first effect-eligible context");
        assert!(matches!(v62_attempt, ReliableInputWireAttempt::Traced(_)));
        assert_eq!(consumed_again, Some(context));
    }

    #[test]
    fn traced_reliable_input_generation_race_restores_context_and_preserves_fifo() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 49, 35);
        mux.add_pane(&pane)
            .expect("register codec-generation race pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture codec-generation race pane registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x74; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        let first_request = ReliableKeyEventV1 {
            pane_id: 35,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('u'),
                modifiers: KeyModifiers::CTRL,
            },
            input_serial: InputSerial::from_millis_since_epoch(44),
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        let second_request = ReliableKeyEventV1 {
            pane_id: 35,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('u'),
                modifiers: KeyModifiers::CTRL,
            },
            input_serial: InputSerial::from_millis_since_epoch(45),
            kind: ReliableKeyEventKindV1::KeyUp,
        };
        let context = sampled_reliable_key_context();
        inner
            .reliable_input_queue
            .enqueue_with_trace_context(
                &inner,
                registration.clone(),
                Arc::clone(&pane_authority),
                first_request.clone(),
                Some(context),
                None,
            )
            .expect("sampled first input queues under v62");
        inner
            .reliable_input_queue
            .enqueue(&inner, registration, pane_authority, second_request.clone())
            .expect("successor input queues behind sampled first input");
        let first = inner.reliable_input_queue.state.lock().pending[0].clone();

        inner
            .reliable_input_queue
            .arm_after_claim_generation_barrier(
                peer.clone(),
                RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION - 1,
            );
        let raced = promise::spawn::block_on(ReliableInputQueue::attempt(
            &inner.reliable_input_queue,
            &inner,
            &first,
        ));
        assert!(matches!(
            raced,
            ReliableInputAttempt::Retry(delay, "transport_retired")
                if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
        ));
        assert!(
            peer.is_empty(),
            "the retired exact v62 scope must never redirect PDU98 onto its v61 successor"
        );
        assert_eq!(
            inner.client.agreed_codec_version(),
            Some(RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION - 1)
        );
        assert_eq!(
            inner
                .client
                .rpc_scope()
                .connection_generation()
                .map(std::num::NonZeroU64::get),
            Some(2),
            "the retry must observe the replacement v61 generation"
        );
        assert_eq!(
            inner.reliable_input_queue.state.lock().pending[0].key_trace_context(),
            Some(context),
            "definitely-not-sent local rejection restores the sampled context"
        );

        let (first_outcome, first_wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &first),
            peer.respond_next_reliable_applied(),
        ));
        let first_wire = first_wire.expect("v61 peer applies the first PDU96");
        assert!(matches!(
            first_outcome,
            ReliableInputAttempt::Complete("applied")
        ));
        assert!(!first_wire.traced);
        assert_eq!(first_wire.request.input_serial, first_request.input_serial);
        assert_eq!(
            first_wire.request.pane_registration,
            Some(pane_registration)
        );
        assert!(inner.reliable_input_queue.complete_front(&first, "applied"));

        let second = inner.reliable_input_queue.state.lock().pending[0].clone();
        let (second_outcome, second_wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &second),
            peer.respond_next_reliable_applied(),
        ));
        let second_wire = second_wire.expect("successor FIFO entry reaches the v61 peer");
        assert!(matches!(
            second_outcome,
            ReliableInputAttempt::Complete("applied")
        ));
        assert!(!second_wire.traced);
        assert_eq!(
            second_wire.request.input_serial,
            second_request.input_serial
        );
        assert!(inner
            .reliable_input_queue
            .complete_front(&second, "applied"));
        assert!(inner.reliable_input_queue.state.lock().pending.is_empty());
        assert!(peer.is_empty());
    }

    #[test]
    fn ambiguous_reliable_key_cannot_cross_server_incarnation() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 156, 142);
        mux.add_pane(&pane)
            .expect("register reliable-key incarnation-fence pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture reliable-key incarnation-fence registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x54; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        inner
            .reliable_input_queue
            .enqueue(
                &inner,
                registration,
                Arc::clone(&pane_authority),
                ReliableKeyEventV1 {
                    pane_id: 142,
                    pane_registration: None,
                    event: KeyEvent {
                        key: KeyCode::Char('i'),
                        modifiers: KeyModifiers::CTRL,
                    },
                    input_serial: InputSerial::from_millis_since_epoch(46),
                    kind: ReliableKeyEventKindV1::KeyDown,
                },
            )
            .expect("incarnation-fenced reliable key queues");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let (original, _) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &entry,
                ReliableInputCodecDisposition::Reliable,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("initial exact server session claims the key");
        let ReliableInputWireAttempt::Unsampled(original) = original else {
            panic!("base reliable-key control must use PDU96");
        };
        assert_eq!(original.pane_registration, Some(pane_registration));
        assert!(inner
            .reliable_input_queue
            .set_front_key_ambiguity(&entry, true));
        assert!(matches!(
            inner.reliable_input_queue.claim_front_wire_attempt(
                &entry,
                ReliableInputCodecDisposition::Reliable,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            ),
            Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt)
        ));

        assert!(inner
            .reliable_input_queue
            .set_front_key_ambiguity(&entry, false));
        let (safe_successor_probe, _) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &entry,
                ReliableInputCodecDisposition::Reliable,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("definitely-not-sent key may safely re-probe a successor server");
        let ReliableInputWireAttempt::Unsampled(safe_successor_probe) = safe_successor_probe else {
            panic!("successor authority probe must use PDU96");
        };
        assert_eq!(safe_successor_probe.pane_registration, None);
        assert_eq!(cached_pane_registration(&pane_authority), None);
    }

    fn test_client_inner(local_domain_id: DomainId) -> Arc<ClientInner> {
        let unix = UnixDomain {
            name: "client-pane-test".to_string(),
            ..UnixDomain::default()
        };
        Arc::new(ClientInner::new(
            local_domain_id,
            Client::new_test_client(Some(local_domain_id), ClientDomainConfig::Unix(unix)),
            None,
            None,
            false,
        ))
    }

    fn test_client_inner_with_rpc_peer(
        local_domain_id: DomainId,
    ) -> (Arc<ClientInner>, TestRpcPeer) {
        let unix = UnixDomain {
            name: "client-pane-rpc-test".to_string(),
            ..UnixDomain::default()
        };
        let (client, peer) = Client::new_test_client_with_rpc_peer(
            Some(local_domain_id),
            ClientDomainConfig::Unix(unix),
        );
        (
            Arc::new(ClientInner::new(local_domain_id, client, None, None, false)),
            peer,
        )
    }

    fn test_client_pane(
        inner: &Arc<ClientInner>,
        local_pane_id: PaneId,
        remote_pane_id: PaneId,
    ) -> Arc<ClientPane> {
        let pane = Arc::new(ClientPane::new(
            inner,
            local_pane_id,
            23,
            remote_pane_id,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));
        pane.prepare_render_application_bootstrap(&inner.client.rpc_scope())
            .expect("test pane should prepare its committed render connection");
        pane
    }

    #[test]
    fn rejected_interactive_inputs_do_not_publish_local_prediction_authority() {
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 40, 29);
        let before = pane.renderable.lock().inner.borrow().input_serial;

        pane.key_down(KeyCode::Char('x'), KeyModifiers::NONE)
            .expect_err("closed test transport must reject input during exact admission");

        let after = pane.renderable.lock().inner.borrow().input_serial;
        assert_eq!(
            after, before,
            "rejected input must not advance prediction authority"
        );

        pane.send_paste("paste")
            .expect_err("closed test transport must reject paste during exact admission");

        let after_paste = pane.renderable.lock().inner.borrow().input_serial;
        assert_eq!(
            after_paste, before,
            "rejected paste must not advance prediction authority"
        );
    }

    #[test]
    fn legacy_key_dispatch_generation_swap_cannot_redirect_onto_reliable_successor() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(&inner.client, LEGACY46_CODEC_VERSION)
            .expect("install exact legacy key generation");
        let pane = test_client_pane(&inner, 153, 139);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register exact-generation key-dispatch pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        for kind in [
            ReliableKeyEventKindV1::KeyDown,
            ReliableKeyEventKindV1::KeyUp,
        ] {
            inner
                .reliable_input_queue
                .arm_after_interactive_scope_capture_generation_barrier(
                    peer.clone(),
                    CODEC_VERSION_MIN_SUPPORTED,
                );
            let result = match kind {
                ReliableKeyEventKindV1::KeyDown => {
                    pane.key_down(KeyCode::Char('g'), KeyModifiers::CTRL)
                }
                ReliableKeyEventKindV1::KeyUp => {
                    pane.key_up(KeyCode::Char('g'), KeyModifiers::CTRL)
                }
            };
            result.expect_err(
                "retired exact legacy scope must fail admission instead of redirecting onto the supported current dialect",
            );
            assert!(
                peer.is_empty(),
                "legacy PDU must not enter the reliable successor generation"
            );
            assert!(inner.reliable_input_queue.state.lock().pending.is_empty());

            match kind {
                ReliableKeyEventKindV1::KeyDown => pane
                    .key_down(KeyCode::Char('g'), KeyModifiers::CTRL)
                    .expect("retry on the exact current generation queues reliably"),
                ReliableKeyEventKindV1::KeyUp => pane
                    .key_up(KeyCode::Char('g'), KeyModifiers::CTRL)
                    .expect("retry on the exact current generation queues reliably"),
            }
            let queued = inner.reliable_input_queue.state.lock().pending[0].clone();
            let QueuedReliableInputPayload::Key {
                request,
                initial_rpc_scope,
                effect_may_have_reached,
                ..
            } = &queued.payload
            else {
                panic!("current-dialect retry must enter the reliable key lane");
            };
            assert_eq!(request.kind, kind);
            assert_eq!(
                initial_rpc_scope
                    .as_ref()
                    .and_then(RpcGenerationScope::agreed_codec_version),
                Some(CODEC_VERSION_MIN_SUPPORTED)
            );
            assert!(!effect_may_have_reached);
            assert!(inner
                .reliable_input_queue
                .complete_front(&queued, "test_complete"));

            peer.replace_ready_generation(&inner.client, LEGACY46_CODEC_VERSION)
                .expect("restore exact legacy key generation for the next control");
        }
    }

    #[test]
    fn reliable_key_downgrade_before_first_send_preserves_safe_legacy_fallback() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(
            &inner.client,
            RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION,
        )
        .expect("install exact traced reliable-key generation");
        let pane = test_client_pane(&inner, 154, 140);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register reliable-to-legacy key pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        for kind in [
            ReliableKeyEventKindV1::KeyDown,
            ReliableKeyEventKindV1::KeyUp,
        ] {
            match kind {
                ReliableKeyEventKindV1::KeyDown => pane
                    .key_down_with_trace_context(
                        KeyCode::Char('d'),
                        KeyModifiers::ALT,
                        sampled_reliable_key_context(),
                    )
                    .expect("traced key-down enters the exact reliable lane"),
                ReliableKeyEventKindV1::KeyUp => pane
                    .key_up_with_trace_context(
                        KeyCode::Char('d'),
                        KeyModifiers::ALT,
                        sampled_reliable_key_context(),
                    )
                    .expect("traced key-up enters the exact reliable lane"),
            }
            let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
            inner
                .reliable_input_queue
                .arm_after_claim_generation_barrier(peer.clone(), LEGACY46_CODEC_VERSION);
            let raced = promise::spawn::block_on(ReliableInputQueue::attempt(
                &inner.reliable_input_queue,
                &inner,
                &entry,
            ));
            assert!(matches!(
                raced,
                ReliableInputAttempt::Retry(delay, "transport_retired")
                    if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
            ));
            assert!(peer.is_empty());
            let legacy_generation = inner
                .client
                .rpc_scope()
                .connection_generation()
                .expect("legacy test transport has an exact generation");
            let (legacy, consumed_context) = inner
                .reliable_input_queue
                .claim_front_legacy_key_attempt(&entry, legacy_generation)
                .expect("definitely-not-sent reliable attempt permits one legacy effect");
            assert_eq!(consumed_context, Some(sampled_reliable_key_context()));
            assert!(matches!(
                (kind, legacy),
                (
                    ReliableKeyEventKindV1::KeyDown,
                    LegacyKeyWireAttempt::KeyDown(_)
                ) | (
                    ReliableKeyEventKindV1::KeyUp,
                    LegacyKeyWireAttempt::KeyUp(_)
                )
            ));
            assert!(inner
                .reliable_input_queue
                .complete_front(&entry, "test_complete"));

            peer.replace_ready_generation(
                &inner.client,
                RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION,
            )
            .expect("restore reliable generation for the next downgrade control");
        }
    }

    #[test]
    fn definitely_not_sent_retry_preserves_prior_reliable_key_ambiguity() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(
            &inner.client,
            RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION,
        )
        .expect("install exact traced reliable-key generation");
        let pane = test_client_pane(&inner, 157, 143);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register cumulative-ambiguity key pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        pane.key_down_with_trace_context(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
            sampled_reliable_key_context(),
        )
        .expect("traced key enters the exact reliable lane");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x55; 16]);
        assert!(inner.reliable_input_queue.bind_front_pane_authority(
            &entry,
            pane_authority_binding(
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
                pane_registration,
            ),
        ));
        let (prior_effect_attempt, _) = inner
            .reliable_input_queue
            .claim_front_wire_attempt(
                &entry,
                ReliableInputCodecDisposition::ReliableTraced,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("first authority-bound attempt reaches uncertain transport");
        let ReliableInputWireAttempt::Traced(prior_effect_attempt) = prior_effect_attempt else {
            panic!("first effect-eligible attempt must carry its sampled context");
        };
        assert_eq!(
            prior_effect_attempt.request.pane_registration,
            Some(pane_registration)
        );
        assert!(inner
            .reliable_input_queue
            .set_front_key_ambiguity(&entry, true));

        inner
            .reliable_input_queue
            .arm_after_claim_generation_barrier(peer.clone(), LEGACY46_CODEC_VERSION);
        let raced = promise::spawn::block_on(ReliableInputQueue::attempt(
            &inner.reliable_input_queue,
            &inner,
            &entry,
        ));
        assert!(matches!(
            raced,
            ReliableInputAttempt::Retry(delay, "transport_retired")
                if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
        ));
        assert!(peer.is_empty());
        let legacy_generation = inner
            .client
            .rpc_scope()
            .connection_generation()
            .expect("legacy test transport has an exact generation");
        assert!(matches!(
            inner
                .reliable_input_queue
                .claim_front_legacy_key_attempt(&entry, legacy_generation),
            Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt)
        ));

        // Once an effect-capable attempt originates on the same legacy
        // transport, neither a same-generation retry nor a successor may
        // replay it: the former remains ambiguous and the latter has lost the
        // only physical-connection fence available to codec 46.
        assert!(inner
            .reliable_input_queue
            .set_front_key_ambiguity(&entry, false));
        {
            let mut state = inner.reliable_input_queue.state.lock();
            let QueuedReliableInputPayload::Key {
                attempted_authority,
                ..
            } = &mut state.pending[0].payload
            else {
                panic!("test entry must remain a queued key");
            };
            *attempted_authority = None;
        }
        inner
            .reliable_input_queue
            .claim_front_legacy_key_attempt(&entry, legacy_generation)
            .expect("first legacy attempt is admitted on its exact transport");
        assert!(inner
            .reliable_input_queue
            .set_front_key_ambiguity(&entry, true));
        assert!(matches!(
            inner
                .reliable_input_queue
                .claim_front_legacy_key_attempt(&entry, legacy_generation),
            Err(ReliableKeyClaimError::ReliableEffectMayHaveReached)
        ));
        let successor_generation = NonZeroU64::new(
            legacy_generation
                .get()
                .checked_add(1)
                .expect("test transport generation can advance"),
        )
        .expect("advanced test transport generation is nonzero");
        assert!(matches!(
            inner
                .reliable_input_queue
                .claim_front_legacy_key_attempt(&entry, successor_generation),
            Err(ReliableKeyClaimError::ServerRestartAfterAmbiguousAttempt)
        ));
    }

    #[test]
    fn sampled_paste_uses_pdu99_once_and_preserves_operational_bytes() {
        let _scope = MuxTestScope::enter();
        let executor = promise::spawn::ScopedExecutor::new();
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane = test_client_pane(&inner, 40, 29);
        let payload = "private paste bytes\nwith ordering";

        pane.send_paste_with_trace_context(payload, sampled_paste_context())
            .expect("sampled paste should enter the live test transport");
        let request = promise::spawn::block_on(executor.run(peer.respond_next_unit()))
            .expect("test peer should receive sampled paste");
        let Pdu::SendPasteTracedV1(traced) = request else {
            panic!("sampled paste used unexpected wire request");
        };
        assert_eq!(traced.request.pane_id, 29);
        assert_eq!(traced.request.data, payload);
        assert!(!traced.request.input_serial.is_empty());
        assert_eq!(traced.trace_context.path, InteractionTracePath::Paste);
        assert!(peer.is_empty());
    }

    #[test]
    fn sampled_paste_generation_swaps_never_split_dialect_from_rpc_scope() {
        let _scope = MuxTestScope::enter();
        let executor = promise::spawn::ScopedExecutor::new();
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane = test_client_pane(&inner, 155, 141);
        let payload = "generation-bound paste";

        peer.replace_ready_generation(&inner.client, SEND_PASTE_TRACED_V1_MIN_CODEC_VERSION)
            .expect("install exact traced-paste generation");
        let before_downgrade = pane.renderable.lock().inner.borrow().input_serial;
        inner
            .reliable_input_queue
            .arm_after_interactive_scope_capture_generation_barrier(
                peer.clone(),
                SEND_PASTE_TRACED_V1_MIN_CODEC_VERSION - 1,
            );
        pane.send_paste_with_trace_context(payload, sampled_paste_context())
            .expect_err(
                "retired traced scope must not redirect its PDU99 decision onto a base successor",
            );
        assert!(peer.is_empty());
        assert_eq!(
            pane.renderable.lock().inner.borrow().input_serial,
            before_downgrade,
            "failed exact-scope admission must not publish paste prediction authority"
        );

        pane.send_paste_with_trace_context(payload, sampled_paste_context())
            .expect("retry on the exact base generation must remain operational");
        let base = promise::spawn::block_on(executor.run(peer.respond_next_unit()))
            .expect("base successor receives the explicit retry");
        let Pdu::SendPaste(base) = base else {
            panic!("pre-trace successor must receive only PDU13");
        };
        assert_eq!(base.data, payload);

        let before_upgrade = pane.renderable.lock().inner.borrow().input_serial;
        inner
            .reliable_input_queue
            .arm_after_interactive_scope_capture_generation_barrier(
                peer.clone(),
                SEND_PASTE_TRACED_V1_MIN_CODEC_VERSION,
            );
        pane.send_paste_with_trace_context(payload, sampled_paste_context())
            .expect_err(
                "retired base scope must not redirect its PDU13 decision onto a traced successor",
            );
        assert!(peer.is_empty());
        assert_eq!(
            pane.renderable.lock().inner.borrow().input_serial,
            before_upgrade,
            "failed upgrade-race admission must not publish paste prediction authority"
        );

        pane.send_paste_with_trace_context(payload, sampled_paste_context())
            .expect("retry on the exact traced generation must use PDU99");
        let traced = promise::spawn::block_on(executor.run(peer.respond_next_unit()))
            .expect("traced successor receives the explicit retry");
        let Pdu::SendPasteTracedV1(traced) = traced else {
            panic!("traced successor must receive PDU99");
        };
        assert_eq!(traced.request.data, payload);
        assert_eq!(traced.trace_context, sampled_paste_context());
        assert!(peer.is_empty());
    }

    #[test]
    fn reliable_input_queue_has_a_hard_bound_and_retains_fifo_identity() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 43, 29);
        let pane_for_mux: Arc<dyn Pane> = pane;
        mux.add_pane(&pane_for_mux)
            .expect("reliable-input queue test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("reliable-input queue test needs exact pane authority");
        {
            let mut state = inner.reliable_input_queue.state.lock();
            // Keep this unit test entirely off the transport worker while it
            // exercises bounded admission through the production method.
            state.worker_running = true;
        }
        let pane_authority = Arc::new(Mutex::new(None));

        for offset in 0..RELIABLE_INPUT_QUEUE_CAPACITY {
            inner
                .reliable_input_queue
                .enqueue(
                    &inner,
                    registration.clone(),
                    Arc::clone(&pane_authority),
                    ReliableKeyEventV1 {
                        pane_id: 29,
                        pane_registration: None,
                        event: KeyEvent {
                            key: KeyCode::Char('q'),
                            modifiers: KeyModifiers::NONE,
                        },
                        input_serial: InputSerial::from_millis_since_epoch(
                            u64::try_from(offset).unwrap() + 1,
                        ),
                        kind: if offset % 2 == 0 {
                            ReliableKeyEventKindV1::KeyDown
                        } else {
                            ReliableKeyEventKindV1::KeyUp
                        },
                    },
                )
                .expect("every slot through the exact fixed capacity must admit");
        }
        let rejected = inner.reliable_input_queue.enqueue(
            &inner,
            registration,
            Arc::clone(&pane_authority),
            ReliableKeyEventV1 {
                pane_id: 29,
                pane_registration: None,
                event: KeyEvent {
                    key: KeyCode::Char('q'),
                    modifiers: KeyModifiers::NONE,
                },
                input_serial: InputSerial::from_millis_since_epoch(
                    u64::try_from(RELIABLE_INPUT_QUEUE_CAPACITY).unwrap() + 1,
                ),
                kind: ReliableKeyEventKindV1::KeyDown,
            },
        );
        assert!(rejected.is_err(), "capacity plus one must fail closed");

        let first = {
            let state = inner.reliable_input_queue.state.lock();
            assert_eq!(state.pending.len(), RELIABLE_INPUT_QUEUE_CAPACITY);
            assert_eq!(
                state
                    .pending
                    .front()
                    .unwrap()
                    .key_request()
                    .unwrap()
                    .input_serial
                    .get(),
                1
            );
            assert_eq!(
                state
                    .pending
                    .back()
                    .unwrap()
                    .key_request()
                    .unwrap()
                    .input_serial
                    .get(),
                u64::try_from(RELIABLE_INPUT_QUEUE_CAPACITY).unwrap()
            );
            assert_eq!(
                state.pending.front().unwrap().key_request().unwrap().kind,
                ReliableKeyEventKindV1::KeyDown
            );
            assert_eq!(
                state.pending.get(1).unwrap().key_request().unwrap().kind,
                ReliableKeyEventKindV1::KeyUp
            );
            state.pending.front().unwrap().clone()
        };
        let remote_authority = ReliablePaneRegistrationIdentityV1::from_bytes([0x61; 16]);
        assert!(inner.reliable_input_queue.bind_front_pane_authority(
            &first,
            pane_authority_binding(
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
                remote_authority,
            ),
        ));
        assert_eq!(
            cached_pane_registration(&pane_authority),
            Some(remote_authority)
        );
        let mut state = inner.reliable_input_queue.state.lock();
        assert!(
            state.pending.iter().all(|queued| queued
                .key_request()
                .unwrap()
                .pane_registration
                .is_none()),
            "authority binding must stay O(1) instead of rewriting the queued burst"
        );
        state.pending.clear();
        state.pending_bytes = 0;
        state.worker_running = false;
    }

    #[test]
    fn pane_write_admission_bounds_each_chunk_and_charges_in_flight_ownership() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 143, 129);
        mux.add_pane(&pane)
            .expect("register bounded pane-write pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture bounded pane-write registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_authority = Arc::new(Mutex::new(None));
        let delivery = ReliablePaneWriteDelivery::new();
        let oversized = vec![0x5a; MAX_RELIABLE_PANE_WRITE_DATA_BYTES * 2];

        let accepted = inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration.clone(),
                129,
                Arc::clone(&pane_authority),
                Arc::clone(&delivery),
                &oversized,
            )
            .expect("one bounded prefix must transfer queue ownership");
        assert_eq!(accepted, MAX_RELIABLE_PANE_WRITE_DATA_BYTES);
        let first = inner.reliable_input_queue.state.lock().pending[0].clone();
        let before_claim = inner.reliable_input_queue.state.lock().pending_bytes;
        inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &first,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("in-flight request claim must retain queue ownership");
        {
            let state = inner.reliable_input_queue.state.lock();
            assert_eq!(state.pending.len(), 1);
            assert_eq!(state.pending_bytes, before_claim);
            assert_eq!(
                state.pending_bytes,
                RELIABLE_PANE_WRITE_ENTRY_OVERHEAD_BYTES + accepted
            );
        }

        while inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration.clone(),
                129,
                Arc::clone(&pane_authority),
                Arc::clone(&delivery),
                &oversized,
            )
            .is_ok()
        {}
        let state = inner.reliable_input_queue.state.lock();
        assert!(state.pending.len() < RELIABLE_INPUT_QUEUE_CAPACITY);
        assert!(state.pending_bytes <= RELIABLE_INPUT_QUEUE_BYTE_CAPACITY);
        assert_eq!(
            state.pending_bytes,
            state
                .pending
                .iter()
                .map(QueuedReliableInput::estimated_bytes)
                .sum::<usize>()
        );
        assert_eq!(delivery.pending_chunks(), state.pending.len());
        drop(state);
        inner.reliable_input_queue.retire("domain_detached");
    }

    #[test]
    fn applied_partial_suffix_stays_ahead_of_later_key_and_write_entries() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 144, 130);
        mux.add_pane(&pane).expect("register partial-write pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture partial-write registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x44; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        let first_delivery = ReliablePaneWriteDelivery::new();
        let later_delivery = ReliablePaneWriteDelivery::new();

        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration.clone(),
                130,
                Arc::clone(&pane_authority),
                Arc::clone(&first_delivery),
                b"abcdef",
            )
            .expect("first pane write queues");
        inner
            .reliable_input_queue
            .enqueue(
                &inner,
                registration.clone(),
                Arc::clone(&pane_authority),
                ReliableKeyEventV1 {
                    pane_id: 130,
                    pane_registration: None,
                    event: KeyEvent {
                        key: KeyCode::Char('k'),
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::empty(),
                    kind: ReliableKeyEventKindV1::KeyDown,
                },
            )
            .expect("later key queues behind the pane write");
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                130,
                Arc::clone(&pane_authority),
                later_delivery,
                b"later",
            )
            .expect("later pane write queues behind the key");

        let (first, first_serial, reserved_end, key_serial, bytes_before) = {
            let state = inner.reliable_input_queue.state.lock();
            let first = state.pending[0].clone();
            let QueuedReliableInputPayload::PaneWrite {
                request,
                reserved_serial_end,
                ..
            } = &first.payload
            else {
                panic!("FIFO front must be the first pane write");
            };
            (
                first.clone(),
                request.input_serial,
                *reserved_serial_end,
                state.pending[1]
                    .key_request()
                    .expect("later key remains queued behind the pane write")
                    .input_serial,
                state.pending_bytes,
            )
        };
        let first_wire = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &first,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("first write attempt records its exact server incarnation");
        assert_eq!(first_wire.pane_registration, Some(pane_registration));
        assert!(!inner
            .reliable_input_queue
            .apply_front_pane_write_prefix(&first, 2)
            .expect("two-byte applied prefix is authoritative"));

        let state = inner.reliable_input_queue.state.lock();
        assert_eq!(state.pending.len(), 3);
        let QueuedReliableInputPayload::PaneWrite {
            request: suffix, ..
        } = &state.pending[0].payload
        else {
            panic!("partial suffix must remain the FIFO front");
        };
        assert_eq!(suffix.data, b"cdef");
        assert_eq!(suffix.input_serial.get(), first_serial.get() + 2);
        assert!(suffix.input_serial <= reserved_end);
        assert!(suffix.input_serial < key_serial);
        assert_eq!(
            state.pending[1]
                .key_request()
                .map(|request| request.input_serial),
            Some(key_serial)
        );
        assert!(matches!(
            &state.pending[2].payload,
            QueuedReliableInputPayload::PaneWrite { .. }
        ));
        assert_eq!(state.pending_bytes, bytes_before - 2);
        assert_eq!(first_delivery.pending_chunks(), 1);
        drop(state);
        let suffix_entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let suffix_after_restart = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &suffix_entry,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("an acknowledged suffix may safely re-probe a replacement server");
        assert_eq!(suffix_after_restart.data, b"cdef");
        assert_eq!(
            suffix_after_restart.input_serial.get(),
            first_serial.get() + 2
        );
        assert_eq!(suffix_after_restart.pane_registration, None);
        assert_eq!(*pane_authority.lock(), None);
        inner.reliable_input_queue.retire("domain_detached");
    }

    #[test]
    fn pane_write_response_prefix_larger_than_exact_request_quarantines_without_slicing() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 145, 131);
        mux.add_pane(&pane).expect("register invalid-prefix pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture invalid-prefix registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x45; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        let delivery = ReliablePaneWriteDelivery::new();
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                131,
                pane_authority,
                Arc::clone(&delivery),
                b"abc",
            )
            .expect("exact three-byte request queues");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();

        let (attempt, wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_reliable_pane_write(ReliablePaneWriteOutcomeV1::AppliedPrefix {
                bytes: 4,
            }),
        ));
        let wire = wire.expect("test peer receives exact pane-write request");
        assert_eq!(wire.request.data, b"abc");
        assert_eq!(wire.request.pane_registration, Some(pane_registration));
        assert!(matches!(
            attempt,
            ReliableInputAttempt::DropOne("outcome_indeterminate")
        ));
        {
            let state = inner.reliable_input_queue.state.lock();
            let QueuedReliableInputPayload::PaneWrite { request, .. } = &state.pending[0].payload
            else {
                panic!("invalid prefix must leave the exact request quarantinable");
            };
            assert_eq!(request.data, b"abc");
        }
        assert!(inner.reliable_input_queue.fail_front(
            &entry,
            "outcome_indeterminate",
            ReliablePaneWriteFailure::Indeterminate,
        ));
        assert_eq!(delivery.pending_chunks(), 0);
        assert_eq!(
            delivery.sticky_failure(),
            Some(ReliablePaneWriteFailure::Indeterminate)
        );
    }

    #[test]
    fn nonterminal_pane_write_retries_preserve_exact_identity_and_data() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 146, 132);
        mux.add_pane(&pane).expect("register retry-identity pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture retry-identity registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x46; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        let delivery = ReliablePaneWriteDelivery::new();
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                132,
                pane_authority,
                delivery,
                b"same-request",
            )
            .expect("retry request queues");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();

        let (first_attempt, first_wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_reliable_pane_write(ReliablePaneWriteOutcomeV1::Retry(
                ReliablePaneWriteRetryV1::DefinitelyNotApplied { retry_after_ns: 1 },
            )),
        ));
        assert!(matches!(
            first_attempt,
            ReliableInputAttempt::Retry(_, "write_zero")
        ));
        let first_wire = first_wire
            .expect("first retry request reaches peer")
            .request;

        let (second_attempt, second_wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_reliable_pane_write(ReliablePaneWriteOutcomeV1::Retry(
                ReliablePaneWriteRetryV1::Input(ReliableKeyEventRetryV1::DuplicatePending {
                    retry_after_ns: 1,
                }),
            )),
        ));
        assert!(matches!(
            second_attempt,
            ReliableInputAttempt::Retry(_, "duplicate_pending")
        ));
        let second_wire = second_wire
            .expect("duplicate-pending retry reaches peer")
            .request;
        assert_eq!(second_wire, first_wire);
        let state = inner.reliable_input_queue.state.lock();
        let QueuedReliableInputPayload::PaneWrite {
            request,
            effect_may_have_reached,
            ..
        } = &state.pending[0].payload
        else {
            panic!("nonterminal retry must retain the pane-write entry");
        };
        assert_eq!(request.data, b"same-request");
        assert_eq!(request.input_serial, first_wire.input_serial);
        assert!(*effect_may_have_reached);
    }

    #[test]
    fn ambiguous_pane_write_cannot_cross_server_incarnation() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 147, 133);
        mux.add_pane(&pane)
            .expect("register incarnation-fence pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture incarnation-fence registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x47; 16]);
        let pane_authority = test_pane_authority(pane_registration);
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                133,
                Arc::clone(&pane_authority),
                ReliablePaneWriteDelivery::new(),
                b"incarnation",
            )
            .expect("incarnation-fenced request queues");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let original = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &entry,
                TEST_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("initial session claims the request");
        assert!(inner
            .reliable_input_queue
            .set_front_pane_write_ambiguity(&entry, true));
        let same_server_route = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &entry,
                ROUTE_FAILOVER_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("route failover within one server incarnation may retry exact identity");
        assert_eq!(same_server_route, original);
        assert!(matches!(
            inner.reliable_input_queue.claim_front_pane_write_attempt(
                &entry,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            ),
            Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt)
        ));

        assert!(inner
            .reliable_input_queue
            .set_front_pane_write_ambiguity(&entry, false));
        let proven_safe = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &entry,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("proven-not-sent retry may re-probe a restarted server");
        assert_eq!(proven_safe.pane_registration, None);
        assert_eq!(*pane_authority.lock(), None);
        assert_eq!(proven_safe.data, original.data);
        assert_eq!(proven_safe.input_serial, original.input_serial);
    }

    #[test]
    fn legacy_pane_write_returns_before_reply_and_fences_ambiguous_replay() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(&inner.client, LEGACY46_CODEC_VERSION)
            .expect("install the exact codec-46 transport generation");
        let pane = test_client_pane(&inner, 158, 144);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register legacy pane-write transport fence pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        assert_eq!(
            std::io::Write::write(&mut *pane.writer.lock(), b"legacy")
                .expect("legacy write must transfer bounded FIFO ownership without waiting"),
            6
        );
        assert!(
            peer.is_empty(),
            "the caller must return before the main-thread worker constructs PDU9"
        );
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let (attempt, wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_unit(),
        ));
        assert!(matches!(
            attempt,
            ReliableInputAttempt::Complete("legacy_applied")
        ));
        assert_eq!(
            wire.expect("codec-46 peer receives the exact legacy write"),
            Pdu::WriteToPane(WriteToPane {
                pane_id: 144,
                data: b"legacy".to_vec(),
            })
        );
        assert!(inner
            .reliable_input_queue
            .complete_front(&entry, "legacy_applied"));
        assert_eq!(
            std::io::Write::write(&mut *pane.writer.lock(), b"ambiguous")
                .expect("second codec-46 write transfers FIFO ownership"),
            9
        );
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let generation = inner
            .client
            .rpc_scope()
            .connection_generation()
            .expect("codec-46 transport has an exact nonzero generation");
        let request = inner
            .reliable_input_queue
            .claim_front_legacy_pane_write_attempt(&entry, generation)
            .expect("one exact legacy transport may claim the write");
        assert_eq!(request.pane_id, 144);
        assert_eq!(request.data, b"ambiguous");
        assert!(inner
            .reliable_input_queue
            .set_front_pane_write_ambiguity(&entry, true));
        assert!(matches!(
            inner
                .reliable_input_queue
                .claim_front_legacy_pane_write_attempt(&entry, generation),
            Err(ReliablePaneWriteClaimError::ReliableEffectMayHaveReached)
        ));
        let successor_generation = NonZeroU64::new(
            generation
                .get()
                .checked_add(1)
                .expect("test transport generation can advance"),
        )
        .expect("advanced test transport generation remains nonzero");
        assert!(matches!(
            inner
                .reliable_input_queue
                .claim_front_legacy_pane_write_attempt(&entry, successor_generation),
            Err(ReliablePaneWriteClaimError::ServerRestartAfterAmbiguousAttempt)
        ));
        inner.reliable_input_queue.retire("test_complete");
    }

    #[test]
    fn fresh_pane_write_reprobes_cached_authority_after_server_restart() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 152, 138);
        mux.add_pane(&pane)
            .expect("register fresh successor-reprobe pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture fresh successor-reprobe registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let stale_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x52; 16]);
        let pane_authority = test_pane_authority(stale_registration);
        let delivery = ReliablePaneWriteDelivery::new();
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                138,
                Arc::clone(&pane_authority),
                delivery,
                b"successor",
            )
            .expect("fresh accepted write queues behind a stale session-bound cache");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();

        let successor_probe = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &entry,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("a never-attempted write may safely probe the successor server");
        assert_eq!(successor_probe.pane_registration, None);
        assert_eq!(successor_probe.data, b"successor");
        assert_eq!(cached_pane_registration(&pane_authority), None);

        let successor_registration = ReliablePaneRegistrationIdentityV1::from_bytes([0x53; 16]);
        assert!(inner.reliable_input_queue.bind_front_pane_authority(
            &entry,
            pane_authority_binding(
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
                successor_registration,
            ),
        ));
        let successor_effect_attempt = inner
            .reliable_input_queue
            .claim_front_pane_write_attempt(
                &entry,
                SERVER_RESTART_RENDER_CONNECTION_IDENTITY.session_incarnation,
            )
            .expect("successor-bound retry may carry only the successor registration");
        assert_eq!(
            successor_effect_attempt.pane_registration,
            Some(successor_registration)
        );
        assert_ne!(
            successor_effect_attempt.pane_registration,
            Some(stale_registration)
        );
        inner.reliable_input_queue.retire("test_complete");
    }

    #[test]
    fn pane_writer_flush_is_nonblocking_on_mux_thread_and_waits_without_queue_lock_off_thread() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 148, 134);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register flush-test pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        assert_eq!(
            std::io::Write::write(&mut *pane.writer.lock(), b"pending")
                .expect("pending bytes transfer bounded ownership"),
            7
        );
        for _ in 0..2 {
            let error = std::io::Write::flush(&mut *pane.writer.lock())
                .expect_err("mux-main-thread flush must never block its own scheduler");
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        }
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let pane_for_flush = Arc::clone(&pane);
        let flush_thread = std::thread::spawn(move || {
            result_tx
                .send(std::io::Write::flush(&mut *pane_for_flush.writer.lock()))
                .expect("flush result receiver stays live");
        });
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(
            inner.reliable_input_queue.state.try_lock().is_some(),
            "off-thread flush must wait without retaining the reliable-input queue mutex"
        );
        assert!(inner
            .reliable_input_queue
            .complete_front(&entry, "test_applied"));
        result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("settled flush must wake")
            .expect("successful settlement must flush cleanly");
        flush_thread.join().expect("flush waiter exits");

        assert_eq!(
            std::io::Write::write(&mut *pane.writer.lock(), b"quarantine")
                .expect("second bounded write queues"),
            10
        );
        let failed = inner.reliable_input_queue.state.lock().pending[0].clone();
        assert!(inner.reliable_input_queue.fail_front(
            &failed,
            "outcome_indeterminate",
            ReliablePaneWriteFailure::Indeterminate,
        ));
        for _ in 0..2 {
            let error = std::io::Write::flush(&mut *pane.writer.lock())
                .expect_err("terminal delivery failure must remain sticky");
            assert_eq!(error.kind(), std::io::ErrorKind::Other);
        }
        let error = std::io::Write::write(&mut *pane.writer.lock(), b"must-not-queue")
            .expect_err("sticky quarantine must reject subsequent writer ownership");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(inner.reliable_input_queue.state.lock().pending.is_empty());
    }

    #[test]
    fn pane_write_delivery_terminal_state_blocks_clean_flush_and_readmission_atomically() {
        let delivery = ReliablePaneWriteDelivery::new();
        delivery
            .try_accept_chunk()
            .expect("clean delivery accepts its first bounded chunk");
        delivery.finish_chunk(Some(ReliablePaneWriteFailure::Indeterminate));

        assert_eq!(
            delivery.flush_pending(),
            Err(ReliablePaneWriteFailure::Indeterminate),
            "terminal settlement must never be projected as a clean zero-pending flush"
        );
        assert_eq!(
            delivery.try_accept_chunk(),
            Err(ReliablePaneWriteFailure::Indeterminate),
            "sticky quarantine must linearize against concurrent new admission"
        );
        assert_eq!(delivery.pending_chunks(), 0);
    }

    #[test]
    fn terminal_pane_write_failure_retires_later_owned_chunks_but_preserves_keys() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 150, 136);
        mux.add_pane(&pane).expect("register terminal-stream pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture terminal-stream pane registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_authority =
            test_pane_authority(ReliablePaneRegistrationIdentityV1::from_bytes([0x50; 16]));
        let delivery = ReliablePaneWriteDelivery::new();

        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration.clone(),
                136,
                Arc::clone(&pane_authority),
                Arc::clone(&delivery),
                b"first",
            )
            .expect("first writer chunk queues");
        inner
            .reliable_input_queue
            .enqueue(
                &inner,
                registration.clone(),
                Arc::clone(&pane_authority),
                ReliableKeyEventV1 {
                    pane_id: 136,
                    pane_registration: None,
                    event: KeyEvent {
                        key: KeyCode::Char('u'),
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::empty(),
                    kind: ReliableKeyEventKindV1::KeyUp,
                },
            )
            .expect("key-up queues between writer chunks");
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                136,
                pane_authority,
                Arc::clone(&delivery),
                b"must-not-follow",
            )
            .expect("later writer chunk transfers ownership before terminal settlement");
        assert_eq!(delivery.pending_chunks(), 2);
        let failed = inner.reliable_input_queue.state.lock().pending[0].clone();

        assert!(inner.reliable_input_queue.fail_pane_write_stream(
            &failed,
            "outcome_indeterminate",
            ReliablePaneWriteFailure::Indeterminate,
        ));
        let state = inner.reliable_input_queue.state.lock();
        assert_eq!(state.pending.len(), 1);
        assert!(state.pending[0].key_request().is_some());
        assert_eq!(state.pending_bytes, RELIABLE_KEY_INPUT_ESTIMATED_BYTES);
        drop(state);
        assert_eq!(delivery.pending_chunks(), 0);
        assert_eq!(
            delivery.sticky_failure(),
            Some(ReliablePaneWriteFailure::Indeterminate)
        );
        inner.reliable_input_queue.retire("test_complete");
    }

    #[test]
    fn pane_write_codec_upgrade_race_uses_one_exact_generation_authority() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 151, 137);
        mux.add_pane(&pane)
            .expect("register pane-write codec-upgrade race pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture pane-write codec-upgrade race registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_authority =
            test_pane_authority(ReliablePaneRegistrationIdentityV1::from_bytes([0x51; 16]));
        let delivery = ReliablePaneWriteDelivery::new();

        peer.activate_reconnect_generation(&inner.client)
            .expect("install an unready generation for bounded queue admission");
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                137,
                pane_authority,
                Arc::clone(&delivery),
                b"upgrade",
            )
            .expect("an unready transport may retain bounded pane bytes");
        peer.replace_ready_generation(&inner.client, RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION - 1)
            .expect("install the exact pre-v64 generation at the audit cut");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        inner
            .reliable_input_queue
            .arm_after_pane_write_scope_capture_generation_barrier(
                peer.clone(),
                RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION,
            );

        let raced = promise::spawn::block_on(ReliableInputQueue::attempt(
            &inner.reliable_input_queue,
            &inner,
            &entry,
        ));
        assert!(matches!(
            raced,
            ReliableInputAttempt::Retry(delay, "connection_identity_unavailable")
                if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
        ));
        assert_eq!(
            inner.client.agreed_codec_version(),
            Some(RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION),
            "the successor v64 generation must remain authoritative"
        );
        assert!(
            peer.is_empty(),
            "the retired pre-v64 scope must not enqueue PDU100 on its successor"
        );
        assert_eq!(delivery.pending_chunks(), 1);
        assert_eq!(delivery.sticky_failure(), None);

        let (settled, wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_reliable_pane_write(ReliablePaneWriteOutcomeV1::AppliedPrefix {
                bytes: 7,
            }),
        ));
        assert!(matches!(
            settled,
            ReliableInputAttempt::Complete("applied_prefix")
        ));
        let wire = wire.expect("the exact successor generation receives the retained bytes");
        assert_eq!(wire.request.data, b"upgrade");
        assert_eq!(wire.request.pane_id, 137);
        assert!(inner
            .reliable_input_queue
            .complete_front(&entry, "applied_prefix"));
        assert_eq!(delivery.pending_chunks(), 0);
        assert_eq!(delivery.sticky_failure(), None);
    }

    #[test]
    fn accepted_pane_write_survives_temporary_pre_v64_generation() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(&inner.client, RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION)
            .expect("install exact v64 admission generation");
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 157, 143);
        mux.add_pane(&pane)
            .expect("register temporary codec-downgrade pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture temporary codec-downgrade registration");
        inner.reliable_input_queue.state.lock().worker_running = true;
        let pane_authority =
            test_pane_authority(ReliablePaneRegistrationIdentityV1::from_bytes([0x55; 16]));
        let delivery = ReliablePaneWriteDelivery::new();
        inner
            .reliable_input_queue
            .enqueue_pane_write(
                &inner,
                registration,
                143,
                pane_authority,
                Arc::clone(&delivery),
                b"retained",
            )
            .expect("v64 admission transfers exact byte ownership");
        let entry = inner.reliable_input_queue.state.lock().pending[0].clone();
        inner
            .reliable_input_queue
            .arm_after_pane_write_scope_capture_generation_barrier(
                peer.clone(),
                RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION - 1,
            );

        let retired_scope = promise::spawn::block_on(ReliableInputQueue::attempt(
            &inner.reliable_input_queue,
            &inner,
            &entry,
        ));
        assert!(matches!(
            retired_scope,
            ReliableInputAttempt::Retry(delay, "connection_identity_unavailable")
                if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
        ));
        let pre_v64 = promise::spawn::block_on(ReliableInputQueue::attempt(
            &inner.reliable_input_queue,
            &inner,
            &entry,
        ));
        assert!(matches!(
            pre_v64,
            ReliableInputAttempt::Retry(delay, "unsupported_codec")
                if delay == RELIABLE_INPUT_TRANSPORT_RETRY_DELAY
        ));
        assert!(peer.is_empty());
        assert_eq!(delivery.pending_chunks(), 1);
        assert_eq!(delivery.sticky_failure(), None);

        peer.replace_ready_generation(&inner.client, RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION)
            .expect("restore a capable exact generation");
        let (settled, wire) = promise::spawn::block_on(futures::future::join(
            ReliableInputQueue::attempt(&inner.reliable_input_queue, &inner, &entry),
            peer.respond_next_reliable_pane_write(ReliablePaneWriteOutcomeV1::AppliedPrefix {
                bytes: 8,
            }),
        ));
        assert!(matches!(
            settled,
            ReliableInputAttempt::Complete("applied_prefix")
        ));
        assert_eq!(
            wire.expect("restored v64 peer receives retained bytes")
                .request
                .data,
            b"retained"
        );
        assert!(inner
            .reliable_input_queue
            .complete_front(&entry, "applied_prefix"));
        assert_eq!(delivery.pending_chunks(), 0);
        assert_eq!(delivery.sticky_failure(), None);
    }

    #[test]
    fn pane_writer_pre_v64_admission_rejection_recovers_after_peer_upgrade() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let (inner, peer) = test_client_inner_with_rpc_peer(17);
        peer.replace_ready_generation(&inner.client, RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION - 1)
            .expect("install an explicit v63 test peer");
        let pane = test_client_pane(&inner, 149, 135);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("register old-codec pane");
        inner.reliable_input_queue.state.lock().worker_running = true;

        let error = std::io::Write::write(&mut *pane.writer.lock(), b"no-shim")
            .expect_err("v63 peer must fail explicitly instead of using WriteToPane");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        let repeated = std::io::Write::write(&mut *pane.writer.lock(), b"still-no-shim")
            .expect_err("v63 remains explicitly unsupported while it is authoritative");
        assert_eq!(repeated.kind(), std::io::ErrorKind::Unsupported);
        assert!(inner.reliable_input_queue.state.lock().pending.is_empty());
        assert!(peer.is_empty());

        peer.replace_ready_generation(&inner.client, RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION)
            .expect("upgrade the exact peer generation to v64");
        let upgraded = b"after-upgrade";
        assert_eq!(
            std::io::Write::write(&mut *pane.writer.lock(), upgraded)
                .expect("zero-byte v63 refusals must not permanently quarantine the writer"),
            upgraded.len()
        );
        assert_eq!(inner.reliable_input_queue.state.lock().pending.len(), 1);
        assert_eq!(pane.writer.lock().delivery.sticky_failure(), None);
        inner.reliable_input_queue.retire("test_complete");
    }

    #[test]
    fn reliable_input_scheduler_rejection_precedes_queue_mutation() {
        let scope = MuxTestScope::enter();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 44, 30);
        let pane_for_mux: Arc<dyn Pane> = pane;
        mux.add_pane(&pane_for_mux)
            .expect("reliable-input rejection test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("reliable-input rejection test needs exact pane authority");

        let error = inner
            .reliable_input_queue
            .enqueue(
                &inner,
                registration,
                Arc::new(Mutex::new(None)),
                ReliableKeyEventV1 {
                    pane_id: 30,
                    pane_registration: None,
                    event: KeyEvent {
                        key: KeyCode::Char('r'),
                        modifiers: KeyModifiers::NONE,
                    },
                    input_serial: InputSerial::from_millis_since_epoch(1),
                    kind: ReliableKeyEventKindV1::KeyDown,
                },
            )
            .expect_err("the test harness has no bounded main-thread generation");
        assert!(error.to_string().contains("before queue mutation"));
        let state = inner.reliable_input_queue.state.lock();
        assert!(state.pending.is_empty());
        assert!(!state.worker_running);
    }

    #[test]
    fn reliable_input_domain_detach_atomically_retires_and_closes_admission() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane: Arc<dyn Pane> = test_client_pane(&inner, 45, 31);
        mux.add_pane(&pane).expect("register detach test pane");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("capture detach test pane registration");
        {
            let mut state = inner.reliable_input_queue.state.lock();
            state.worker_running = true;
        }
        let pane_authority = Arc::new(Mutex::new(None));
        let request = ReliableKeyEventV1 {
            pane_id: 31,
            pane_registration: None,
            event: KeyEvent {
                key: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
            },
            input_serial: InputSerial::from_millis_since_epoch(1),
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        inner
            .reliable_input_queue
            .enqueue(
                &inner,
                registration.clone(),
                Arc::clone(&pane_authority),
                request.clone(),
            )
            .expect("pre-detach input should be retained");

        inner.mark_detached();
        assert!(inner.is_detached());
        {
            let state = inner.reliable_input_queue.state.lock();
            assert!(state.domain_detached);
            assert!(state.pending.is_empty());
            assert!(!state.worker_running);
        }
        assert!(
            inner
                .reliable_input_queue
                .enqueue(&inner, registration, pane_authority, request)
                .is_err(),
            "detached-domain admission must remain closed after retirement"
        );
    }

    #[test]
    fn reliable_input_pane_authority_retirement_is_exact_registration_scoped() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let first_pane: Arc<dyn Pane> = test_client_pane(&inner, 43, 29);
        let second_pane: Arc<dyn Pane> = test_client_pane(&inner, 44, 30);
        mux.add_pane(&first_pane)
            .expect("register first client pane");
        mux.add_pane(&second_pane)
            .expect("register second client pane");
        let first_registration = mux
            .capture_pane_registration(&first_pane)
            .expect("capture first exact client pane registration");
        let second_registration = mux
            .capture_pane_registration(&second_pane)
            .expect("capture second exact client pane registration");
        let first_authority = ReliablePaneRegistrationIdentityV1::from_bytes([0x61; 16]);
        let second_authority = ReliablePaneRegistrationIdentityV1::from_bytes([0x62; 16]);
        let first_authority_cache = test_pane_authority(first_authority);
        let second_authority_cache = test_pane_authority(second_authority);
        let request = |pane_id, pane_registration, input_serial| ReliableKeyEventV1 {
            pane_id,
            pane_registration: Some(pane_registration),
            event: KeyEvent {
                key: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
            },
            input_serial: InputSerial::from_millis_since_epoch(input_serial),
            kind: ReliableKeyEventKindV1::KeyDown,
        };
        let first = QueuedReliableInput {
            registration: first_registration,
            pane_authority: Arc::clone(&first_authority_cache),
            payload: QueuedReliableInputPayload::Key {
                request: request(29, first_authority, 1),
                trace_context: None,
                initial_rpc_scope: None,
                attempted_authority: None,
                effect_may_have_reached: false,
            },
        };
        let second = QueuedReliableInput {
            registration: second_registration,
            pane_authority: Arc::clone(&second_authority_cache),
            payload: QueuedReliableInputPayload::Key {
                request: request(30, second_authority, 2),
                trace_context: None,
                initial_rpc_scope: None,
                attempted_authority: None,
                effect_may_have_reached: false,
            },
        };
        {
            let mut state = inner.reliable_input_queue.state.lock();
            state.pending_bytes = first
                .estimated_bytes()
                .saturating_add(second.estimated_bytes());
            state.pending.push_back(first.clone());
            state.pending.push_back(second.clone());
            state.worker_running = true;
        }

        assert!(inner
            .reliable_input_queue
            .retire_front_pane_authority(&first, "pane_registration_mismatch"));
        assert_eq!(*first_authority_cache.lock(), None);
        assert_eq!(
            cached_pane_registration(&second_authority_cache),
            Some(second_authority)
        );
        let state = inner.reliable_input_queue.state.lock();
        assert_eq!(state.pending.len(), 1);
        assert!(
            state.pending[0]
                .registration
                .same_registration(&second.registration),
            "retiring one pane generation must preserve another pane's FIFO work"
        );
        assert_eq!(state.pending[0].key_request(), second.key_request());
    }

    fn test_render_application_update(
        connection_generation: u64,
        pane_id: PaneId,
        scheduler_sequence: u64,
        attempt: u64,
        kind: RenderApplicationKind,
        base_state: Option<RenderStateIdentity>,
        state_sequence: u64,
    ) -> RenderApplicationUpdate {
        const RENDER_GENERATION: u64 = 101;
        let surface_sequence =
            usize::try_from(state_sequence).expect("test state sequence should fit SequenceNo");
        let bonus_lines = if kind == RenderApplicationKind::Snapshot {
            (0isize..24)
                .map(|row| {
                    let line = if row == 0 {
                        Line::from_text("ready", &CellAttributes::default(), surface_sequence, None)
                    } else {
                        Line::with_width(80, surface_sequence)
                    };
                    (row, line)
                })
                .collect()
        } else {
            vec![(
                0,
                Line::from_text("ready", &CellAttributes::default(), surface_sequence, None),
            )]
        };
        RenderApplicationUpdate {
            identity: RenderApplicationIdentity {
                protocol_version: RENDER_APPLICATION_PROTOCOL_VERSION,
                token: RenderApplicationToken {
                    connection_generation,
                    coordinator_instance: 103,
                    scheduler_sequence,
                    attempt,
                    ledger_instance: 107,
                    render_generation: RENDER_GENERATION,
                    ledger_obligation: scheduler_sequence,
                },
                pane_id,
                base_state,
                resulting_state: RenderStateIdentity {
                    render_generation: RENDER_GENERATION,
                    state_sequence,
                },
                kind,
            },
            retry_budget: RenderApplicationRetryBudget {
                attempt_ordinal: u16::try_from(attempt)
                    .expect("test attempt should fit the retry ordinal"),
                max_attempts: 3,
                remaining_millis: 250,
            },
            surface: GetPaneRenderChangesResponse {
                pane_id,
                mouse_grabbed: true,
                alt_screen_active: true,
                cursor_position: StableCursorPosition::default(),
                dimensions: RenderableDimensions {
                    cols: 80,
                    viewport_rows: 24,
                    scrollback_rows: 24,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: 96,
                    pixel_width: 800,
                    pixel_height: 480,
                    reverse_video: false,
                },
                tiered_scrollback_status: None,
                dirty_lines: std::iter::once(0..1).collect(),
                title: "render-application".to_string(),
                working_dir: None,
                bonus_lines: SerializedLines::from(bonus_lines),
                input_serial: None,
                seqno: surface_sequence,
            },
            semantic_zones: if kind == RenderApplicationKind::Snapshot {
                RenderComponentUpdate::Replace(GetSemanticZonesResponse {
                    pane_id,
                    zones: Vec::new(),
                    zone_texts: Vec::new(),
                    last_exit_code: None,
                })
            } else {
                RenderComponentUpdate::Unchanged
            },
            palette: if kind == RenderApplicationKind::Snapshot {
                RenderComponentUpdate::Replace(SetPalette {
                    pane_id,
                    palette: Arc::new(ColorPalette::default()),
                })
            } else {
                RenderComponentUpdate::Unchanged
            },
            alerts: Vec::new(),
            connection_identity: TEST_RENDER_CONNECTION_IDENTITY,
        }
    }

    fn settlement(disposition: ClientRenderApplicationDisposition) -> RenderApplicationResult {
        match disposition {
            ClientRenderApplicationDisposition::Settlement(result) => result,
            ClientRenderApplicationDisposition::DuplicateInProgress => {
                panic!("test expected a terminal render-application settlement")
            }
            ClientRenderApplicationDisposition::ProtocolViolation(error) => {
                panic!(
                    "test expected a settlement, got protocol violation: {}",
                    error
                )
            }
        }
    }

    struct NoopClipboard;

    impl Clipboard for NoopClipboard {
        fn set_contents(
            &self,
            _selection: wezterm_term::ClipboardSelection,
            _data: Option<String>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct ReentrantClipboard {
        pane: std::sync::Weak<ClientPane>,
        callback_ran: Arc<AtomicBool>,
    }

    impl Clipboard for ReentrantClipboard {
        fn set_contents(
            &self,
            _selection: wezterm_term::ClipboardSelection,
            _data: Option<String>,
        ) -> anyhow::Result<()> {
            let pane = self
                .pane
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("test client pane was dropped"))?;
            let clipboard_guard = pane.clipboard.try_lock().ok_or_else(|| {
                anyhow::anyhow!("clipboard callback ran while the ClientPane mutex was held")
            })?;
            drop(clipboard_guard);

            let replacement: Arc<dyn Clipboard> = Arc::new(NoopClipboard);
            pane.set_clipboard(&replacement);
            self.callback_ran.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[test]
    fn render_application_acks_only_after_atomic_apply_and_deduplicates_retry() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::Alert { pane_id: 40, alert } = notification {
                observed_for_subscriber.lock().unwrap().push(alert);
            }
            true
        })
        .expect("render-application subscription should allocate an identifier");

        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 40, 29);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("render-application test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("render-application test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();
        let connection_generation = rpc
            .connection_generation()
            .expect("test RPC scope should carry an exact generation")
            .get();

        let mut update = test_render_application_update(
            connection_generation,
            pane.remote_pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        let semantic_zone = SemanticZone {
            start_y: 0,
            start_x: 0,
            end_y: 0,
            end_x: 5,
            semantic_type: SemanticType::Prompt,
        };
        update.semantic_zones = RenderComponentUpdate::Replace(GetSemanticZonesResponse {
            pane_id: pane.remote_pane_id,
            zones: vec![semantic_zone],
            zone_texts: vec!["ready".to_string()],
            last_exit_code: Some(0),
        });
        let application_palette = ColorPalette::default();
        update.palette = RenderComponentUpdate::Replace(SetPalette {
            pane_id: pane.remote_pane_id,
            palette: Arc::new(application_palette.clone()),
        });
        update.alerts = vec![
            NotifyAlert {
                pane_id: pane.remote_pane_id,
                alert: Alert::OutputSinceFocusLost,
            },
            NotifyAlert {
                pane_id: pane.remote_pane_id,
                alert: Alert::Progress(Progress::Percentage(64)),
            },
        ];
        update
            .validate()
            .expect("complete snapshot fixture should satisfy the wire contract");

        let result = promise::spawn::block_on(pane.apply_render_application(
            &registration,
            &rpc,
            update.clone(),
        ));
        settlement(result)
            .validate_for(&update)
            .expect("ACK must bind the fully applied update");

        assert_eq!(pane.get_current_seqno(), 1);
        assert_eq!(pane.get_title(), "render-application");
        assert!(pane.is_mouse_grabbed());
        assert!(pane.is_alt_screen_active());
        assert!(pane.has_unseen_output());
        assert_eq!(pane.get_progress(), Progress::Percentage(64));
        assert_eq!(pane.get_semantic_zones().unwrap(), vec![semantic_zone]);
        assert_eq!(
            pane.get_text_from_semantic_zone(semantic_zone).unwrap(),
            "ready"
        );
        assert_eq!(pane.get_semantic_exit_code().unwrap(), Some(0));
        assert_eq!(*pane.palette.lock(), application_palette);
        let (_, lines) = pane.get_lines(0..1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].as_str(), "ready");
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                Alert::PaletteChanged,
                Alert::OutputSinceFocusLost,
                Alert::Progress(Progress::Percentage(64)),
            ],
            "each atomic component must become observable exactly once before ACK"
        );

        let mut retry = update.clone();
        retry.identity.token.attempt = 2;
        retry.retry_budget.attempt_ordinal = 2;
        let retry_result = promise::spawn::block_on(pane.apply_render_application(
            &registration,
            &rpc,
            retry.clone(),
        ));
        settlement(retry_result)
            .validate_for(&retry)
            .expect("idempotent retry ACK must bind the retry attempt identity");
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                Alert::PaletteChanged,
                Alert::OutputSinceFocusLost,
                Alert::Progress(Progress::Percentage(64)),
            ],
            "an already-applied logical update must never replay event-like alerts"
        );
        assert_eq!(
            pane.render_application_counters(),
            ClientRenderApplicationCounters {
                applications_started: 1,
                acknowledgements: 1,
                duplicate_acknowledgements: 1,
                duplicate_in_progress: 0,
                nacks: 0,
                cancelled_attempts: 0,
            }
        );

        let reset_snapshot = test_render_application_update(
            connection_generation,
            pane.remote_pane_id,
            113,
            1,
            RenderApplicationKind::Snapshot,
            None,
            2,
        );
        let reset_result = promise::spawn::block_on(pane.apply_render_application(
            &registration,
            &rpc,
            reset_snapshot.clone(),
        ));
        settlement(reset_result)
            .validate_for(&reset_snapshot)
            .expect("authoritative snapshot without state alerts should ACK");
        assert!(!pane.has_unseen_output());
        assert_eq!(pane.get_progress(), Progress::None);
    }

    #[test]
    fn render_application_state_rejects_gap_stale_generation_and_wrong_pane() {
        let connection_generation = 11;
        let pane_id = 29;
        let snapshot = test_render_application_update(
            connection_generation,
            pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            4,
        );
        let state = Mutex::new(ClientRenderApplicationState::default());
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                snapshot.connection_identity,
                snapshot.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                observed_state: RenderApplicationObservedState::NotApplicable,
            }
        ));
        assert!(
            state.lock().prepare_authoritative_bootstrap(
                TEST_RENDER_CONNECTION_IDENTITY,
                connection_generation,
            ),
            "the first committed connection must require an authoritative snapshot"
        );
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                snapshot.connection_identity,
                snapshot.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        ClientRenderApplicationGuard::new(&state, snapshot.connection_identity, snapshot.identity)
            .acknowledge();

        let gap = test_render_application_update(
            connection_generation,
            pane_id,
            113,
            1,
            RenderApplicationKind::Delta,
            Some(RenderStateIdentity {
                render_generation: 101,
                state_sequence: 6,
            }),
            7,
        );
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                gap.connection_identity,
                gap.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::DetectedGap,
                observed_state: RenderApplicationObservedState::Applied(RenderStateIdentity {
                    render_generation: 101,
                    state_sequence: 4,
                }),
            }
        ));

        let stale = test_render_application_update(
            connection_generation,
            pane_id,
            127,
            1,
            RenderApplicationKind::Delta,
            Some(RenderStateIdentity {
                render_generation: 101,
                state_sequence: 3,
            }),
            5,
        );
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                stale.connection_identity,
                stale.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::BaseMismatch,
                observed_state: RenderApplicationObservedState::Applied(RenderStateIdentity {
                    render_generation: 101,
                    state_sequence: 4,
                }),
            }
        ));

        let next = test_render_application_update(
            connection_generation,
            pane_id,
            131,
            1,
            RenderApplicationKind::Delta,
            Some(snapshot.identity.resulting_state),
            5,
        );
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation + 1),
                pane_id,
                next.connection_identity,
                next.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                ..
            }
        ));
        assert!(matches!(
            state.lock().begin(
                Some(SUCCESSOR_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                snapshot.connection_identity,
                snapshot.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::GenerationMismatch,
                ..
            }
        ));
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id + 1,
                next.connection_identity,
                next.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::MalformedOrIncomplete {
                    component: RenderApplicationComponent::Surface,
                },
                observed_state: RenderApplicationObservedState::NotApplicable,
            }
        ));

        let successor_generation = connection_generation + 1;
        assert!(
            state.lock().prepare_authoritative_bootstrap(
                SUCCESSOR_RENDER_CONNECTION_IDENTITY,
                successor_generation,
            ),
            "a successor connection must reset inherited render authority"
        );
        assert_eq!(
            state.lock().observation(),
            RenderApplicationObservedState::Uninitialized,
            "a successor must not inherit the predecessor baseline"
        );

        let mut reconnect_delta = test_render_application_update(
            successor_generation,
            pane_id,
            137,
            1,
            RenderApplicationKind::Delta,
            Some(snapshot.identity.resulting_state),
            5,
        );
        reconnect_delta.connection_identity = SUCCESSOR_RENDER_CONNECTION_IDENTITY;
        assert!(matches!(
            state.lock().begin(
                Some(SUCCESSOR_RENDER_CONNECTION_IDENTITY),
                Some(successor_generation),
                pane_id,
                reconnect_delta.connection_identity,
                reconnect_delta.identity,
            ),
            ClientRenderApplicationBegin::Nack {
                reason: RenderApplicationNackReason::BaseMismatch,
                observed_state: RenderApplicationObservedState::Uninitialized,
            }
        ));

        let mut reconnect_snapshot = test_render_application_update(
            successor_generation,
            pane_id,
            139,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        reconnect_snapshot.connection_identity = SUCCESSOR_RENDER_CONNECTION_IDENTITY;
        assert!(matches!(
            state.lock().begin(
                Some(SUCCESSOR_RENDER_CONNECTION_IDENTITY),
                Some(successor_generation),
                pane_id,
                reconnect_snapshot.connection_identity,
                reconnect_snapshot.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        ClientRenderApplicationGuard::new(
            &state,
            reconnect_snapshot.connection_identity,
            reconnect_snapshot.identity,
        )
        .acknowledge();
        assert_eq!(
            state.lock().applied_state,
            Some(reconnect_snapshot.identity.resulting_state),
            "the authoritative successor snapshot may restart its sequence below the predecessor"
        );
    }

    #[test]
    fn render_application_in_progress_retry_is_coalesced_and_cancellation_safe() {
        let connection_generation = 11;
        let pane_id = 29;
        let first = test_render_application_update(
            connection_generation,
            pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        let mut retry = first.clone();
        retry.identity.token.attempt = 2;
        retry.retry_budget.attempt_ordinal = 2;
        let state = Mutex::new(ClientRenderApplicationState::default());
        assert!(state.lock().prepare_authoritative_bootstrap(
            TEST_RENDER_CONNECTION_IDENTITY,
            connection_generation,
        ));
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                first.connection_identity,
                first.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        let guard =
            ClientRenderApplicationGuard::new(&state, first.connection_identity, first.identity);
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                retry.connection_identity,
                retry.identity,
            ),
            ClientRenderApplicationBegin::DuplicateInProgress
        ));
        drop(guard);

        {
            let state = state.lock();
            assert!(state.applying.is_none());
            assert_eq!(state.counters.applications_started, 1);
            assert_eq!(state.counters.duplicate_in_progress, 1);
            assert_eq!(state.counters.cancelled_attempts, 1);
        }
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(connection_generation),
                pane_id,
                retry.connection_identity,
                retry.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
    }

    #[test]
    fn successor_bootstrap_revokes_predecessor_inflight_guard_without_aliasing() {
        let first_generation = 11;
        let successor_generation = 12;
        let pane_id = 29;
        let first = test_render_application_update(
            first_generation,
            pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        let state = Mutex::new(ClientRenderApplicationState::default());
        assert!(state
            .lock()
            .prepare_authoritative_bootstrap(TEST_RENDER_CONNECTION_IDENTITY, first_generation,));
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(first_generation),
                pane_id,
                first.connection_identity,
                first.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        let predecessor_guard =
            ClientRenderApplicationGuard::new(&state, first.connection_identity, first.identity);

        assert!(state.lock().prepare_authoritative_bootstrap(
            SUCCESSOR_RENDER_CONNECTION_IDENTITY,
            successor_generation,
        ));
        predecessor_guard.acknowledge();
        {
            let state = state.lock();
            assert_eq!(
                state.active_connection_identity,
                Some(SUCCESSOR_RENDER_CONNECTION_IDENTITY)
            );
            assert_eq!(
                state.active_connection_generation,
                Some(successor_generation)
            );
            assert!(state.applied_state.is_none());
            assert!(state.applying.is_none());
            assert_eq!(state.counters.cancelled_attempts, 1);
            assert_eq!(state.counters.acknowledgements, 0);
        }

        let mut successor = test_render_application_update(
            successor_generation,
            pane_id,
            113,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        successor.connection_identity = SUCCESSOR_RENDER_CONNECTION_IDENTITY;
        assert!(matches!(
            state.lock().begin(
                Some(SUCCESSOR_RENDER_CONNECTION_IDENTITY),
                Some(successor_generation),
                pane_id,
                successor.connection_identity,
                successor.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        let successor_guard = ClientRenderApplicationGuard::new(
            &state,
            successor.connection_identity,
            successor.identity,
        );
        assert!(
            !state.lock().prepare_authoritative_bootstrap(
                SUCCESSOR_RENDER_CONNECTION_IDENTITY,
                successor_generation,
            ),
            "replaying the same committed topology identity must not cancel live successor work"
        );
        successor_guard.acknowledge();
        let state = state.lock();
        assert_eq!(
            state.applied_state,
            Some(successor.identity.resulting_state)
        );
        assert_eq!(state.counters.cancelled_attempts, 1);
        assert_eq!(state.counters.acknowledgements, 1);
    }

    #[test]
    fn restart_route_failover_and_bootstrap_disconnect_never_inherit_state() {
        let reused_numeric_generation = 1;
        let pane_id = 29;
        let first = test_render_application_update(
            reused_numeric_generation,
            pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            97,
        );
        let state = Mutex::new(ClientRenderApplicationState::default());
        assert!(state.lock().prepare_authoritative_bootstrap(
            TEST_RENDER_CONNECTION_IDENTITY,
            reused_numeric_generation,
        ));
        assert!(matches!(
            state.lock().begin(
                Some(TEST_RENDER_CONNECTION_IDENTITY),
                Some(reused_numeric_generation),
                pane_id,
                first.connection_identity,
                first.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        ClientRenderApplicationGuard::new(&state, first.connection_identity, first.identity)
            .acknowledge();

        assert!(state.lock().prepare_authoritative_bootstrap(
            ROUTE_FAILOVER_RENDER_CONNECTION_IDENTITY,
            reused_numeric_generation,
        ));
        assert_eq!(
            state.lock().observation(),
            RenderApplicationObservedState::Uninitialized,
            "a fresh topology stream must fence numeric generation reuse"
        );
        let mut abandoned_route_snapshot = test_render_application_update(
            reused_numeric_generation,
            pane_id,
            113,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        abandoned_route_snapshot.connection_identity = ROUTE_FAILOVER_RENDER_CONNECTION_IDENTITY;
        assert!(matches!(
            state.lock().begin(
                Some(ROUTE_FAILOVER_RENDER_CONNECTION_IDENTITY),
                Some(reused_numeric_generation),
                pane_id,
                abandoned_route_snapshot.connection_identity,
                abandoned_route_snapshot.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        let abandoned_route_guard = ClientRenderApplicationGuard::new(
            &state,
            abandoned_route_snapshot.connection_identity,
            abandoned_route_snapshot.identity,
        );

        assert!(state.lock().prepare_authoritative_bootstrap(
            SERVER_RESTART_RENDER_CONNECTION_IDENTITY,
            reused_numeric_generation,
        ));
        abandoned_route_guard.acknowledge();
        {
            let state = state.lock();
            assert_eq!(
                state.active_connection_identity,
                Some(SERVER_RESTART_RENDER_CONNECTION_IDENTITY)
            );
            assert_eq!(
                state.observation(),
                RenderApplicationObservedState::Uninitialized,
                "disconnect during route bootstrap must retain no partial baseline"
            );
            assert!(state.applying.is_none());
            assert_eq!(state.counters.cancelled_attempts, 1);
            assert_eq!(state.counters.acknowledgements, 1);
        }

        let stale_predecessor_delta = test_render_application_update(
            reused_numeric_generation,
            pane_id,
            127,
            1,
            RenderApplicationKind::Delta,
            Some(first.identity.resulting_state),
            98,
        );
        for stale in [&first, &stale_predecessor_delta, &abandoned_route_snapshot] {
            assert!(matches!(
                state.lock().begin(
                    Some(SERVER_RESTART_RENDER_CONNECTION_IDENTITY),
                    Some(reused_numeric_generation),
                    pane_id,
                    stale.connection_identity,
                    stale.identity,
                ),
                ClientRenderApplicationBegin::Nack {
                    reason: RenderApplicationNackReason::GenerationMismatch,
                    observed_state: RenderApplicationObservedState::Uninitialized,
                }
            ));
        }

        let mut restarted_server_snapshot = test_render_application_update(
            reused_numeric_generation,
            pane_id,
            131,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        restarted_server_snapshot.connection_identity = SERVER_RESTART_RENDER_CONNECTION_IDENTITY;
        assert!(matches!(
            state.lock().begin(
                Some(SERVER_RESTART_RENDER_CONNECTION_IDENTITY),
                Some(reused_numeric_generation),
                pane_id,
                restarted_server_snapshot.connection_identity,
                restarted_server_snapshot.identity,
            ),
            ClientRenderApplicationBegin::Apply
        ));
        ClientRenderApplicationGuard::new(
            &state,
            restarted_server_snapshot.connection_identity,
            restarted_server_snapshot.identity,
        )
        .acknowledge();
        let state = state.lock();
        assert_eq!(
            state.applied_connection_identity,
            Some(SERVER_RESTART_RENDER_CONNECTION_IDENTITY)
        );
        assert_eq!(
            state.applied_state,
            Some(restarted_server_snapshot.identity.resulting_state)
        );
        assert_eq!(state.counters.cancelled_attempts, 1);
        assert_eq!(state.counters.acknowledgements, 2);
    }

    #[test]
    fn render_application_resource_validation_is_typed_and_bounded() {
        let update = test_render_application_update(
            11,
            29,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );

        let title_limits = ClientRenderApplicationLimits {
            title_bytes: 4,
            ..ClientRenderApplicationLimits::default()
        };
        assert_eq!(
            validate_render_application_resources(&update, title_limits),
            Err(RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Title,
                requested: 18,
                limit: 4,
            })
        );

        let line_limits = ClientRenderApplicationLimits {
            lines: 0,
            ..ClientRenderApplicationLimits::default()
        };
        assert_eq!(
            validate_render_application_resources(&update, line_limits),
            Err(RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Lines,
                requested: 24,
                limit: 0,
            })
        );

        let mut incomplete = update.clone();
        incomplete.surface.bonus_lines = SerializedLines::default();
        assert_eq!(
            validate_render_application_resources(
                &incomplete,
                ClientRenderApplicationLimits::default(),
            ),
            Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Lines,
            })
        );

        let mut semantic = update.clone();
        semantic.semantic_zones = RenderComponentUpdate::Replace(GetSemanticZonesResponse {
            pane_id: 29,
            zones: vec![SemanticZone {
                start_y: 0,
                start_x: 0,
                end_y: 0,
                end_x: 5,
                semantic_type: SemanticType::Output,
            }],
            zone_texts: Vec::new(),
            last_exit_code: None,
        });
        assert_eq!(
            validate_render_application_resources(
                &semantic,
                ClientRenderApplicationLimits::default(),
            ),
            Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::SemanticZones,
            })
        );

        let mut alert = update;
        alert.alerts.push(NotifyAlert {
            pane_id: 29,
            alert: Alert::OutputSinceFocusLost,
        });
        let alert_limits = ClientRenderApplicationLimits {
            supports_alerts: false,
            ..ClientRenderApplicationLimits::default()
        };
        assert_eq!(
            validate_render_application_resources(&alert, alert_limits),
            Err(RenderApplicationNackReason::UnsupportedResource {
                resource: RenderApplicationResource::Alerts,
            })
        );

        alert.alerts.clear();
        alert.alerts.push(NotifyAlert {
            pane_id: 29,
            alert: Alert::WindowTitleChanged("oversize".to_string()),
        });
        let alert_limits = ClientRenderApplicationLimits {
            alert_text_bytes: 4,
            ..ClientRenderApplicationLimits::default()
        };
        assert_eq!(
            validate_render_application_resources(&alert, alert_limits),
            Err(RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Alerts,
                requested: 8,
                limit: 4,
            })
        );
    }

    #[test]
    fn render_application_rejects_wrong_registration_without_mutating_either_pane() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane_a = test_client_pane(&inner, 41, 29);
        let pane_b = test_client_pane(&inner, 42, 30);
        let pane_a_for_mux: Arc<dyn Pane> = pane_a.clone();
        let pane_b_for_mux: Arc<dyn Pane> = pane_b.clone();
        mux.add_pane(&pane_a_for_mux)
            .expect("first render-application registration should bind");
        mux.add_pane(&pane_b_for_mux)
            .expect("second render-application registration should bind");
        let pane_b_registration = mux
            .capture_pane_registration(&pane_b_for_mux)
            .expect("second pane should retain exact registration");
        let rpc = inner.client.rpc_scope();
        let connection_generation = rpc
            .connection_generation()
            .expect("test RPC scope should carry an exact generation")
            .get();
        let update = test_render_application_update(
            connection_generation,
            pane_a.remote_pane_id,
            109,
            1,
            RenderApplicationKind::Snapshot,
            None,
            1,
        );
        let mut invalid_protocol = update.clone();
        invalid_protocol.identity.protocol_version =
            RENDER_APPLICATION_PROTOCOL_VERSION.saturating_add(1);
        assert_eq!(
            promise::spawn::block_on(pane_a.apply_render_application(
                &pane_b_registration,
                &rpc,
                invalid_protocol,
            )),
            ClientRenderApplicationDisposition::ProtocolViolation(
                RenderApplicationContractError::UnsupportedProtocolVersion,
            )
        );

        let result = settlement(promise::spawn::block_on(pane_a.apply_render_application(
            &pane_b_registration,
            &rpc,
            update.clone(),
        )));
        assert_eq!(
            result.outcome,
            RenderApplicationOutcome::Nack(RenderApplicationNack {
                reason: RenderApplicationNackReason::ApplicationFailure {
                    stage: RenderApplicationStage::Commit,
                },
                observed_state: RenderApplicationObservedState::NotApplicable,
            })
        );
        result
            .validate_for(&update)
            .expect("wrong-registration NACK should retain exact attempt identity");
        assert_eq!(pane_a.get_current_seqno(), 0);
        assert_eq!(pane_a.get_title(), "shell");
        assert_eq!(pane_b.get_current_seqno(), 0);
        assert_eq!(pane_b.get_title(), "shell");
        assert_eq!(
            pane_a.render_application_counters(),
            ClientRenderApplicationCounters {
                applications_started: 1,
                acknowledgements: 0,
                duplicate_acknowledgements: 0,
                duplicate_in_progress: 0,
                nacks: 1,
                cancelled_attempts: 0,
            }
        );
    }

    #[test]
    fn render_delta_updates_authoritative_alt_screen_state() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            31,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("render-delta test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("render-delta test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();
        assert!(!pane.is_alt_screen_active());

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
                    pane_id: 29,
                    mouse_grabbed: false,
                    alt_screen_active: true,
                    cursor_position: StableCursorPosition::default(),
                    dimensions: RenderableDimensions {
                        cols: 80,
                        viewport_rows: 24,
                        scrollback_rows: 24,
                        physical_top: 0,
                        scrollback_top: 0,
                        dpi: 96,
                        pixel_width: 800,
                        pixel_height: 480,
                        reverse_video: false,
                    },
                    tiered_scrollback_status: None,
                    dirty_lines: Vec::new(),
                    title: "shell".to_string(),
                    working_dir: None,
                    bonus_lines: SerializedLines::default(),
                    input_serial: None,
                    seqno: 1,
                }),
            )
            .await
        })
        .expect("render delta should apply");

        assert!(pane.is_alt_screen_active());
    }

    #[test]
    fn registration_slot_is_stable_and_rejects_concurrent_mux_owners() {
        let _scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let first_mux = Arc::new(Mux::new(None));
        let second_mux = Arc::new(Mux::new(None));
        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            33,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));

        assert!(
            Arc::ptr_eq(pane.mux_registration_slot(), pane.mux_registration_slot()),
            "a production ClientPane must expose one stable registration slot"
        );

        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        first_mux
            .add_pane(&pane_for_mux)
            .expect("the first mux should bind the ClientPane");
        let slot_registration = pane
            .mux_registration_slot()
            .load()
            .expect("publication must populate the pane-owned slot");
        let registry_registration = first_mux
            .capture_pane_registration(&pane_for_mux)
            .expect("the mux registry must expose the same exact registration");
        assert!(
            slot_registration.same_registration(&registry_registration),
            "the production pane slot and mux registry must carry one generation authority"
        );

        let error = second_mux
            .add_pane(&pane_for_mux)
            .expect_err("one ClientPane object cannot be bound to two mux owners");
        assert!(
            error
                .to_string()
                .contains("already bound to a live or draining mux registration"),
            "unexpected dual-owner rejection: {:#}",
            error,
        );
    }

    #[test]
    fn unilateral_state_alert_is_forwarded_exactly_once() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::Alert { pane_id: 32, alert } = notification {
                observed_for_subscriber.lock().unwrap().push(alert);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let inner = test_client_inner(17);
        let pane = Arc::new(ClientPane::new(
            &inner,
            32,
            23,
            29,
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "shell",
            false,
        ));
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("unilateral-alert test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("unilateral-alert test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::NotifyAlert(NotifyAlert {
                    pane_id: 29,
                    alert: Alert::OutputSinceFocusLost,
                }),
            )
            .await?;
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::NotifyAlert(NotifyAlert {
                    pane_id: 29,
                    alert: Alert::Progress(Progress::Percentage(64)),
                }),
            )
            .await
        })
        .expect("unilateral alerts should apply");

        assert!(*pane.unseen_output.lock());
        assert_eq!(*pane.progress.lock(), Progress::Percentage(64));
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                Alert::OutputSinceFocusLost,
                Alert::Progress(Progress::Percentage(64)),
            ],
            "state mutation and notification forwarding must not emit duplicate alerts"
        );
    }

    #[test]
    fn unilateral_rejects_registration_for_a_different_pane() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::Alert { pane_id, alert } = notification {
                observed_for_subscriber
                    .lock()
                    .unwrap()
                    .push((pane_id, alert));
            }
            true
        })
        .expect("wrong-registration test subscription should allocate an identifier");

        let inner = test_client_inner(17);
        let pane_a = test_client_pane(&inner, 34, 29);
        let pane_b = test_client_pane(&inner, 35, 30);
        let pane_a_for_mux: Arc<dyn Pane> = pane_a.clone();
        let pane_b_for_mux: Arc<dyn Pane> = pane_b.clone();
        mux.add_pane(&pane_a_for_mux)
            .expect("first wrong-registration test pane should register");
        mux.add_pane(&pane_b_for_mux)
            .expect("second wrong-registration test pane should register");
        let pane_b_registration = mux
            .capture_pane_registration(&pane_b_for_mux)
            .expect("second pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        promise::spawn::block_on(async {
            pane_a
                .process_unilateral(
                    &pane_b_registration,
                    &rpc,
                    Pdu::NotifyAlert(NotifyAlert {
                        pane_id: 29,
                        alert: Alert::OutputSinceFocusLost,
                    }),
                )
                .await
        })
        .expect("a wrong registration should be discarded without failing the reader");

        assert!(!*pane_a.unseen_output.lock());
        assert!(!*pane_b.unseen_output.lock());
        assert!(
            observed.lock().unwrap().is_empty(),
            "a registration for pane B must not authorize pane A or emit B-attributed alerts"
        );
    }

    #[test]
    fn unilateral_clipboard_callback_can_reenter_set_clipboard() {
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let mux = Arc::new(Mux::new(None));
        scope.set_mux(&mux);
        let inner = test_client_inner(17);
        let pane = test_client_pane(&inner, 36, 31);
        let pane_for_mux: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&pane_for_mux)
            .expect("clipboard reentrancy test pane should register");
        let registration = mux
            .capture_pane_registration(&pane_for_mux)
            .expect("clipboard reentrancy test pane should retain exact registration");
        let rpc = inner.client.rpc_scope();

        let callback_ran = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<dyn Clipboard> = Arc::new(ReentrantClipboard {
            pane: Arc::downgrade(&pane),
            callback_ran: Arc::clone(&callback_ran),
        });
        pane.set_clipboard(&clipboard);

        promise::spawn::block_on(async {
            pane.process_unilateral(
                &registration,
                &rpc,
                Pdu::SetClipboard(SetClipboard {
                    pane_id: 31,
                    clipboard: Some("copied text".to_string()),
                    selection: wezterm_term::ClipboardSelection::Clipboard,
                }),
            )
            .await
        })
        .expect("clipboard callback should run outside the ClientPane clipboard mutex");

        assert!(
            callback_ran.load(Ordering::Acquire),
            "the clipboard callback should reenter set_clipboard without deadlocking"
        );
    }
}
