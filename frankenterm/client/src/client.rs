use crate::domain::{ClientDomain, ClientDomainConfig, ClientInner};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use asupersync::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use asupersync::runtime::{Interest, IoRegistration};
#[cfg(any(windows, test))]
use asupersync::time::{TimerDriverHandle, TimerHandle};
use asupersync::Cx;
use async_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use async_ossl::AsyncSslStream;
use async_trait::async_trait;
use codec::*;
use config::{configuration, SshDomain, TlsDomainClient, UnixDomain, UnixTarget};
use filedescriptor::FileDescriptor;
use futures::future::{ready, select, Either};
use futures::pin_mut;
use mux::client::ClientId;
use mux::connui::ConnectionUI;
use mux::domain::DomainId;
use mux::pane::{Pane, PaneId};
use mux::ssh::ssh_connect_with_ui;
use mux::{
    DomainOperationGuard, Mux, MuxSessionIncarnation, PaneRegistrationHandle, TopologyRevision,
};
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};
use openssl::x509::X509;
use parking_lot::{Condvar, Mutex as ParkingMutex};
use portable_pty::Child;
use promise::spawn::{
    try_reserve_main_thread, MainThreadReservationOutcome, MainThreadServiceClass,
    MainThreadSpawnReservation,
};
use std::collections::{hash_map::Entry, BTreeMap, HashMap, VecDeque};
use std::convert::TryFrom;
use std::future::{poll_fn, Future};
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::marker::Unpin;
use std::net::TcpStream;
use std::num::NonZeroU64;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context as TaskContext, Poll};
use std::thread;
use std::time::Duration;
use thiserror::Error;
use wezterm_uds::UnixStream;

const UNIX_SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const INITIAL_CONNECTION_GENERATION: u64 = 1;
const MAX_PRE_READY_UNILATERAL_PDUS: usize = 1_024;
const MAX_PRE_READY_UNILATERAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOPOLOGY_FENCE_EVENTS: usize = 4_096;
const MAX_TOPOLOGY_FENCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RPC_READINESS_PARTICIPANTS: usize = 64;
const MAX_RPC_READINESS_PUBLICATIONS: usize = 64;
const MAX_RPC_READINESS_WAITERS: usize = 64;
const MUX_RPC_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(60);
const TOPOLOGY_FENCE_MIN_CODEC_VERSION: usize = 49;
const CLIENT_MAIN_THREAD_TOPOLOGY_ESTIMATED_BYTES: usize = 4 * 1024;
const CLIENT_OUTBOUND_NONINTERACTIVE_SLOTS: usize = 4_096;
const CLIENT_OUTBOUND_RESERVED_SLOTS: usize = 64;
const CLIENT_OUTBOUND_TOTAL_SLOTS: usize =
    CLIENT_OUTBOUND_NONINTERACTIVE_SLOTS + CLIENT_OUTBOUND_RESERVED_SLOTS;
// Safety envelopes, not promoted performance defaults. One legal global-cap
// PDU can require roughly three 256 MiB logical buffers in the conservative
// codec plan. The noninteractive ceiling leaves a separate 64 MiB tranche for
// small control/input frames even while bulk/query/state-sync work is full.
const CLIENT_OUTBOUND_TOTAL_CODEC_BYTES: usize = 1_024 * 1024 * 1024;
const CLIENT_OUTBOUND_NONINTERACTIVE_CODEC_BYTES: usize = 960 * 1024 * 1024;
#[cfg(test)]
pub(crate) const TEST_RENDER_CONNECTION_IDENTITY: RenderConnectionIdentity =
    RenderConnectionIdentity::new(
        TopologyStreamId::from_bytes([0x35; 16]),
        MuxSessionIncarnation::from_bytes([0x57; 16]),
    );

fn reserve_client_main_thread(
    service_class: MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
) -> anyhow::Result<MainThreadSpawnReservation> {
    match try_reserve_main_thread(service_class, estimated_bytes) {
        MainThreadReservationOutcome::Reserved(reservation) => Ok(reservation),
        rejected => Err(anyhow!(
            "main-thread scheduler rejected {operation} before task construction: {rejected:?}"
        )),
    }
}

#[derive(Error, Debug)]
#[error("Timeout")]
struct Timeout;

pub(crate) async fn with_mux_rpc_bootstrap_timeout<T, F>(operation: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    with_mux_rpc_bootstrap_timeout_for(MUX_RPC_BOOTSTRAP_TIMEOUT, operation).await
}

async fn with_mux_rpc_bootstrap_timeout_for<T, F>(
    timeout_duration: Duration,
    operation: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let timeout = async move {
        promise::spawn::sleep(timeout_duration).await;
        Err(anyhow::Error::new(Timeout)).context(format!(
            "mux RPC bootstrap exceeded its {:?} deadline",
            timeout_duration
        ))
    };
    pin_mut!(operation);
    pin_mut!(timeout);
    match select(operation, timeout).await {
        Either::Left((result, _)) | Either::Right((result, _)) => result,
    }
}

/// Poll an already-bound interactive RPC exactly once so that its bounded,
/// non-blocking transport admission happens in the caller's input-dispatch
/// turn rather than waiting behind unrelated main-thread executor work.
///
/// This helper is intentionally limited to the mux RPC futures produced by
/// this module: their first poll performs only validation plus `try_send`, then
/// waits for the reply.  It must not be used for a future whose first poll may
/// block or perform unbounded work.  A pending future remains pinned at the
/// same address and is returned for ordinary asynchronous completion.
pub(crate) fn admit_interactive_rpc_now<F, T>(future: F) -> anyhow::Result<Option<Pin<Box<F>>>>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let mut future = Box::pin(future);
    let waker = futures::task::noop_waker();
    let mut context = TaskContext::from_waker(&waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(Ok(_)) => Ok(None),
        Poll::Ready(Err(error)) => Err(error),
        Poll::Pending => Ok(Some(future)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcDeliveryCertainty {
    DefinitelyNotSent,
    OutcomeUnknown,
}

/// Codec-46 PDU0 contains no trustworthy text, effect, or retry authority.
/// Keep that absence typed all the way through pending-reply correlation;
/// callers receive this terminal error instead of a fabricated current
/// `ErrorResponse`.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("codec-46 mux server rejected {request} without effect or retry authority")]
struct Legacy46RpcRejection {
    request: &'static str,
    effect_authority: Legacy46RejectionAuthority,
    retry_authority: Legacy46RejectionAuthority,
}

impl std::fmt::Display for RpcDeliveryCertainty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefinitelyNotSent => f.write_str("definitely_not_sent"),
            Self::OutcomeUnknown => f.write_str("outcome_unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcRetirementStage {
    Admission,
    Enqueue,
    Queued,
    Dequeue,
    SerialAssignment,
    FrameEncoding,
    BeforeWrite,
    WriteStarted,
    BeforeFlush,
    AfterFlush,
    AwaitingResponse,
    ResponseMatch,
    CompletionChannel,
    ConsumerCommit,
}

impl RpcRetirementStage {
    fn metric_label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Enqueue => "enqueue",
            Self::Queued => "queued",
            Self::Dequeue => "dequeue",
            Self::SerialAssignment => "serial_assignment",
            Self::FrameEncoding => "frame_encoding",
            Self::BeforeWrite => "before_write",
            Self::WriteStarted => "write_started",
            Self::BeforeFlush => "before_flush",
            Self::AfterFlush => "after_flush",
            Self::AwaitingResponse => "awaiting_response",
            Self::ResponseMatch => "response_match",
            Self::CompletionChannel => "completion_channel",
            Self::ConsumerCommit => "consumer_commit",
        }
    }
}

impl std::fmt::Display for RpcRetirementStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.metric_label())
    }
}

/// Stable caller-visible classification for an RPC that could not remain on
/// the exact transport generation where it was admitted.
///
/// No variant authorizes an automatic retry. `OutcomeUnknown` means that a
/// write was attempted and the server may already have observed or committed
/// the operation even though no matching reply reached this client.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum RpcTransportError {
    #[error(
        "mux client RPC transport unavailable for attempt {attempt_id} ({request}) at {stage}"
    )]
    Unavailable {
        attempt_id: NonZeroU64,
        request: &'static str,
        stage: RpcRetirementStage,
    },
    #[error(
        "mux client RPC attempt {attempt_id} ({request}) bound to generation \
         {bound_generation} retired at {stage} with {certainty}; active generation \
         is {active_generation:?}: {reason}"
    )]
    Retired {
        attempt_id: NonZeroU64,
        request: &'static str,
        bound_generation: NonZeroU64,
        active_generation: Option<NonZeroU64>,
        stage: RpcRetirementStage,
        certainty: RpcDeliveryCertainty,
        reason: String,
    },
    #[error("mux client RPC attempt identity space is exhausted for {request}")]
    AttemptIdentityExhausted { request: &'static str },
    #[error(
        "mux client RPC wire serial space is exhausted at attempt {attempt_id} ({request}); \
         this client incarnation is permanently closed"
    )]
    WireSerialExhausted {
        attempt_id: NonZeroU64,
        request: &'static str,
    },
    #[error(
        "mux client connection generation space is exhausted at generation {last_generation}; \
         this client incarnation is permanently closed"
    )]
    ConnectionGenerationExhausted { last_generation: NonZeroU64 },
    #[error(
        "mux client connection generation diverged while retiring generation {retiring_generation}; \
         expected {expected_generation}, observed {observed_generation}; this client incarnation \
         is permanently closed"
    )]
    ConnectionGenerationDiverged {
        retiring_generation: NonZeroU64,
        expected_generation: NonZeroU64,
        observed_generation: u64,
    },
}

impl RpcTransportError {
    /// Whether the transport can prove that no request byte reached the peer.
    ///
    /// Reliable callers may reuse first-attempt-only metadata only for the
    /// `DefinitelyNotSent` class. `OutcomeUnknown` deliberately carries no
    /// such authority even when the operational request itself is retryable.
    #[must_use]
    pub const fn delivery_certainty(&self) -> RpcDeliveryCertainty {
        match self {
            Self::Retired { certainty, .. } => *certainty,
            Self::Unavailable { .. }
            | Self::AttemptIdentityExhausted { .. }
            | Self::WireSerialExhausted { .. }
            | Self::ConnectionGenerationExhausted { .. }
            | Self::ConnectionGenerationDiverged { .. } => RpcDeliveryCertainty::DefinitelyNotSent,
        }
    }

    fn is_incarnation_terminal(&self) -> bool {
        matches!(
            self,
            Self::AttemptIdentityExhausted { .. }
                | Self::WireSerialExhausted { .. }
                | Self::ConnectionGenerationExhausted { .. }
                | Self::ConnectionGenerationDiverged { .. }
        )
    }
}

fn record_rpc_transport_error(error: &RpcTransportError) {
    match error {
        RpcTransportError::Unavailable { stage, .. } => {
            metrics::counter!(
                "mux.client.rpc.transport_unavailable.total",
                "stage" => stage.metric_label()
            )
            .increment(1);
        }
        RpcTransportError::Retired {
            stage, certainty, ..
        } => {
            let certainty = match certainty {
                RpcDeliveryCertainty::DefinitelyNotSent => "definitely_not_sent",
                RpcDeliveryCertainty::OutcomeUnknown => "outcome_unknown",
            };
            metrics::counter!(
                "mux.client.rpc.generation_retirement.total",
                "stage" => stage.metric_label(),
                "certainty" => certainty
            )
            .increment(1);
        }
        RpcTransportError::AttemptIdentityExhausted { .. } => {
            metrics::counter!(
                "mux.client.rpc.transport_unavailable.total",
                "stage" => "attempt_identity_exhausted"
            )
            .increment(1);
        }
        RpcTransportError::WireSerialExhausted { .. } => {
            metrics::counter!(
                "mux.client.rpc.transport_unavailable.total",
                "stage" => "wire_serial_exhausted"
            )
            .increment(1);
        }
        RpcTransportError::ConnectionGenerationExhausted { .. } => {
            metrics::counter!(
                "mux.client.rpc.transport_unavailable.total",
                "stage" => "connection_generation_exhausted"
            )
            .increment(1);
        }
        RpcTransportError::ConnectionGenerationDiverged { .. } => {
            metrics::counter!(
                "mux.client.rpc.transport_unavailable.total",
                "stage" => "connection_generation_diverged"
            )
            .increment(1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcErrorCompletion {
    Delivered,
    Abandoned,
    Full,
}

fn complete_with_rpc_transport_error(
    completion: &Sender<anyhow::Result<PendingRpcReply>>,
    error: RpcTransportError,
) -> RpcErrorCompletion {
    record_rpc_transport_error(&error);
    match completion.try_send(Err(anyhow::Error::new(error))) {
        Ok(()) => RpcErrorCompletion::Delivered,
        Err(TrySendError::Closed(_)) => RpcErrorCompletion::Abandoned,
        Err(TrySendError::Full(_)) => RpcErrorCompletion::Full,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RpcBinding {
    generation: NonZeroU64,
    attempt_id: NonZeroU64,
    request: &'static str,
    expected_response_ident: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClientOutboundBudgetLimits {
    total_codec_bytes: usize,
    noninteractive_codec_bytes: usize,
    total_slots: usize,
    noninteractive_slots: usize,
}

impl Default for ClientOutboundBudgetLimits {
    fn default() -> Self {
        Self {
            total_codec_bytes: CLIENT_OUTBOUND_TOTAL_CODEC_BYTES,
            noninteractive_codec_bytes: CLIENT_OUTBOUND_NONINTERACTIVE_CODEC_BYTES,
            total_slots: CLIENT_OUTBOUND_TOTAL_SLOTS,
            noninteractive_slots: CLIENT_OUTBOUND_NONINTERACTIVE_SLOTS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ClientOutboundBudgetState {
    codec_bytes: usize,
    noninteractive_codec_bytes: usize,
    slots: usize,
    noninteractive_slots: usize,
    peak_codec_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientOutboundBudgetLimit {
    Arithmetic,
    TotalCodecBytes,
    NoninteractiveCodecBytes,
    TotalSlots,
    NoninteractiveSlots,
}

impl ClientOutboundBudgetLimit {
    const fn label(self) -> &'static str {
        match self {
            Self::Arithmetic => "arithmetic",
            Self::TotalCodecBytes => "total_codec_bytes",
            Self::NoninteractiveCodecBytes => "noninteractive_codec_bytes",
            Self::TotalSlots => "total_slots",
            Self::NoninteractiveSlots => "noninteractive_slots",
        }
    }
}

#[derive(Debug, Error)]
#[error(
    "mux client outbound PDU {ident} could not reserve {planned_codec_bytes} logical codec bytes: {limit:?}"
)]
pub struct ClientOutboundAdmissionError {
    pub ident: u64,
    pub planned_codec_bytes: usize,
    pub limit: ClientOutboundBudgetLimit,
}

impl ClientOutboundAdmissionError {
    /// Admission failures occur before serial assignment, queue mutation,
    /// codec allocation, compression, or a socket write.
    #[must_use]
    pub const fn delivery_certainty(&self) -> RpcDeliveryCertainty {
        RpcDeliveryCertainty::DefinitelyNotSent
    }
}

#[derive(Debug)]
struct ClientOutboundBudget {
    limits: ClientOutboundBudgetLimits,
    state: ParkingMutex<ClientOutboundBudgetState>,
}

impl Default for ClientOutboundBudget {
    fn default() -> Self {
        Self {
            limits: ClientOutboundBudgetLimits::default(),
            state: ParkingMutex::new(ClientOutboundBudgetState::default()),
        }
    }
}

impl ClientOutboundBudget {
    #[cfg(test)]
    fn with_limits(limits: ClientOutboundBudgetLimits) -> Self {
        Self {
            limits,
            state: ParkingMutex::new(ClientOutboundBudgetState::default()),
        }
    }

    fn checked_reservation_state(
        &self,
        mut state: ClientOutboundBudgetState,
        planned_codec_bytes: usize,
        noninteractive: bool,
    ) -> Result<ClientOutboundBudgetState, ClientOutboundBudgetLimit> {
        state.codec_bytes = state
            .codec_bytes
            .checked_add(planned_codec_bytes)
            .ok_or(ClientOutboundBudgetLimit::Arithmetic)?;
        if state.codec_bytes > self.limits.total_codec_bytes {
            return Err(ClientOutboundBudgetLimit::TotalCodecBytes);
        }
        state.slots = state
            .slots
            .checked_add(1)
            .ok_or(ClientOutboundBudgetLimit::Arithmetic)?;
        if state.slots > self.limits.total_slots {
            return Err(ClientOutboundBudgetLimit::TotalSlots);
        }
        if noninteractive {
            state.noninteractive_codec_bytes = state
                .noninteractive_codec_bytes
                .checked_add(planned_codec_bytes)
                .ok_or(ClientOutboundBudgetLimit::Arithmetic)?;
            if state.noninteractive_codec_bytes > self.limits.noninteractive_codec_bytes {
                return Err(ClientOutboundBudgetLimit::NoninteractiveCodecBytes);
            }
            state.noninteractive_slots = state
                .noninteractive_slots
                .checked_add(1)
                .ok_or(ClientOutboundBudgetLimit::Arithmetic)?;
            if state.noninteractive_slots > self.limits.noninteractive_slots {
                return Err(ClientOutboundBudgetLimit::NoninteractiveSlots);
            }
        }
        state.peak_codec_bytes = state.peak_codec_bytes.max(state.codec_bytes);
        Ok(state)
    }

    fn try_reserve(
        self: &Arc<Self>,
        rpc_transport: Weak<RpcTransportState>,
        generation: NonZeroU64,
        prepared: OwnedPreparedMuxWirePdu,
    ) -> Result<ClientOutboundLease, ClientOutboundAdmissionError> {
        let metadata = prepared.metadata();
        let request = prepared.pdu().pdu_name();
        let noninteractive = matches!(metadata.queue_qos, PduQueueQos::Normal | PduQueueQos::Bulk);
        let planned_codec_bytes = prepared.codec_peak_bytes();
        let reservation = {
            let mut state = self.state.lock();
            match self.checked_reservation_state(*state, planned_codec_bytes, noninteractive) {
                Ok(next) => {
                    *state = next;
                    Ok(())
                }
                Err(limit) => Err(limit),
            }
        };
        if let Err(limit) = reservation {
            metrics::counter!(
                "mux.client.outbound.admission.total",
                "outcome" => "rejected",
                "qos" => client_outbound_qos_label(metadata.queue_qos),
                "limit" => limit.label(),
            )
            .increment(1);
            return Err(ClientOutboundAdmissionError {
                ident: prepared.ident(),
                planned_codec_bytes,
                limit,
            });
        }
        metrics::counter!(
            "mux.client.outbound.admission.total",
            "outcome" => "admitted",
            "qos" => client_outbound_qos_label(metadata.queue_qos),
            "limit" => "none",
        )
        .increment(1);
        metrics::counter!(
            "mux.client.outbound.codec_bytes.total",
            "outcome" => "reserved",
            "qos" => client_outbound_qos_label(metadata.queue_qos),
        )
        .increment(
            u64::try_from(planned_codec_bytes)
                .expect("bounded client outbound codec bytes fit in u64"),
        );
        Ok(ClientOutboundLease {
            state: Arc::new(ClientOutboundLeaseState {
                phase: AtomicU8::new(ClientOutboundLeasePhase::Queued as u8),
                rollback_armed: AtomicBool::new(false),
                budget: Arc::clone(self),
                rpc_transport,
                generation,
                request,
                ident: prepared.ident(),
                planned_codec_bytes,
                noninteractive,
                qos: metadata.queue_qos,
                prepared: ParkingMutex::new(Some(Box::new(prepared))),
            }),
        })
    }

    #[cfg(test)]
    fn snapshot(&self) -> ClientOutboundBudgetState {
        *self.state.lock()
    }
}

const fn client_outbound_qos_label(qos: PduQueueQos) -> &'static str {
    match qos {
        PduQueueQos::Control => "control",
        PduQueueQos::Interactive => "interactive",
        PduQueueQos::Normal => "normal",
        PduQueueQos::Bulk => "bulk",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ClientOutboundLeasePhase {
    Queued = 0,
    ReaderOwned = 1,
    CanceledQueued = 2,
    Settled = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientOutboundRelease {
    Full,
    PayloadOnly,
    ShellOnly,
}

struct ClientOutboundLeaseState {
    phase: AtomicU8,
    rollback_armed: AtomicBool,
    budget: Arc<ClientOutboundBudget>,
    rpc_transport: Weak<RpcTransportState>,
    generation: NonZeroU64,
    request: &'static str,
    ident: u64,
    planned_codec_bytes: usize,
    noninteractive: bool,
    qos: PduQueueQos,
    prepared: ParkingMutex<Option<Box<OwnedPreparedMuxWirePdu>>>,
}

impl ClientOutboundLeaseState {
    fn release_budget(&self, release: ClientOutboundRelease) {
        let bytes = release != ClientOutboundRelease::ShellOnly;
        let slots = release != ClientOutboundRelease::PayloadOnly;
        {
            let mut state = self.budget.state.lock();
            if bytes {
                state.codec_bytes = state
                    .codec_bytes
                    .checked_sub(self.planned_codec_bytes)
                    .expect("client outbound logical-codec reservation underflow");
            }
            if slots {
                state.slots = state
                    .slots
                    .checked_sub(1)
                    .expect("client outbound slot reservation underflow");
            }
            if bytes && self.noninteractive {
                state.noninteractive_codec_bytes = state
                    .noninteractive_codec_bytes
                    .checked_sub(self.planned_codec_bytes)
                    .expect("client outbound noninteractive-byte reservation underflow");
            }
            if slots && self.noninteractive {
                state.noninteractive_slots = state
                    .noninteractive_slots
                    .checked_sub(1)
                    .expect("client outbound noninteractive-slot reservation underflow");
            }
        }
        if bytes {
            metrics::counter!(
                "mux.client.outbound.codec_bytes.total",
                "outcome" => "released",
                "qos" => client_outbound_qos_label(self.qos),
            )
            .increment(
                u64::try_from(self.planned_codec_bytes)
                    .expect("bounded client outbound codec bytes fit in u64"),
            );
        }
        if release != ClientOutboundRelease::PayloadOnly {
            metrics::counter!(
                "mux.client.outbound.admission.total",
                "outcome" => "released",
                "qos" => client_outbound_qos_label(self.qos),
                "limit" => "none",
            )
            .increment(1);
        }
    }

    fn rollback_protocol_if_armed(&self) {
        if !self.rollback_armed.swap(false, AtomicOrdering::AcqRel) {
            return;
        }
        let Some(rpc_transport) = self.rpc_transport.upgrade() else {
            return;
        };
        if rpc_transport
            .rollback_unadmitted_outbound_ident(self.generation, self.ident)
            .is_err()
        {
            log::trace!(
                "discarding an outbound protocol rollback for retired mux RPC generation {}",
                self.generation
            );
        }
    }

    fn discard_queued_payload(&self) {
        // A canceled queue node retains only its tiny lease/control shell. The
        // potentially large PDU owner is destroyed before its budget is made
        // available to another request.
        drop(self.prepared.lock().take());
        self.rollback_protocol_if_armed();
    }

    fn cancel_if_queued(&self) {
        if self
            .phase
            .compare_exchange(
                ClientOutboundLeasePhase::Queued as u8,
                ClientOutboundLeasePhase::CanceledQueued as u8,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_ok()
        {
            self.discard_queued_payload();
            self.release_budget(ClientOutboundRelease::PayloadOnly);
        }
    }

    fn claim_for_reader(&self) -> anyhow::Result<Option<Box<OwnedPreparedMuxWirePdu>>> {
        if self
            .phase
            .compare_exchange(
                ClientOutboundLeasePhase::Queued as u8,
                ClientOutboundLeasePhase::ReaderOwned as u8,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_err()
        {
            return Ok(None);
        }
        let prepared = self.prepared.lock().take();
        if let Some(prepared) = prepared {
            self.rollback_armed.store(false, AtomicOrdering::Release);
            Ok(Some(prepared))
        } else {
            self.rollback_protocol_if_armed();
            Err(anyhow!(
                "mux client outbound reader claim lost its exact PDU owner"
            ))
        }
    }

    fn settle(&self) {
        let prior = self.phase.swap(
            ClientOutboundLeasePhase::Settled as u8,
            AtomicOrdering::AcqRel,
        );
        if prior != ClientOutboundLeasePhase::Settled as u8 {
            if prior == ClientOutboundLeasePhase::Queued as u8 {
                self.discard_queued_payload();
                self.release_budget(ClientOutboundRelease::Full);
            } else if prior == ClientOutboundLeasePhase::CanceledQueued as u8 {
                self.release_budget(ClientOutboundRelease::ShellOnly);
            } else {
                self.release_budget(ClientOutboundRelease::Full);
            }
        }
    }
}

struct ClientOutboundLease {
    state: Arc<ClientOutboundLeaseState>,
}

struct ClientOutboundCancellationGuard {
    state: Arc<ClientOutboundLeaseState>,
}

impl Drop for ClientOutboundCancellationGuard {
    fn drop(&mut self) {
        self.state.cancel_if_queued();
    }
}

impl std::fmt::Debug for ClientOutboundLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientOutboundLease")
            .field("generation", &self.state.generation)
            .field("request", &self.state.request)
            .field("ident", &self.state.ident)
            .field("planned_codec_bytes", &self.state.planned_codec_bytes)
            .field("qos", &self.state.qos)
            .finish()
    }
}

impl ClientOutboundLease {
    fn cancellation_guard(&self) -> ClientOutboundCancellationGuard {
        ClientOutboundCancellationGuard {
            state: Arc::clone(&self.state),
        }
    }

    fn matches(&self, binding: RpcBinding) -> bool {
        self.state.generation == binding.generation && self.state.request == binding.request
    }

    fn with_prepared<T>(
        &self,
        operation: impl FnOnce(&OwnedPreparedMuxWirePdu) -> T,
    ) -> anyhow::Result<T> {
        let prepared = self.state.prepared.lock();
        let prepared = prepared.as_deref().ok_or_else(|| {
            anyhow!("mux client outbound lease lost its exact PDU before enqueue")
        })?;
        Ok(operation(prepared))
    }

    fn arm_protocol_rollback(&self) {
        if self.state.ident == <GetCodecVersion as PduWireIdent>::IDENT
            || self.state.ident == <SetClientId as PduWireIdent>::IDENT
        {
            self.state
                .rollback_armed
                .store(true, AtomicOrdering::Release);
        }
    }

    fn claim_for_reader(&self) -> anyhow::Result<Option<Box<OwnedPreparedMuxWirePdu>>> {
        self.state.claim_for_reader()
    }
}

impl Drop for ClientOutboundLease {
    fn drop(&mut self) {
        self.state.settle();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcConsumerKind {
    TopologySnapshot,
    RenderBootstrap,
    InitialAttachment,
    InitialAttachmentCleanup,
    SpawnResolution,
    MoveResolution,
    SplitResolution,
    GlobalUnilateral,
    PaneUnilateral,
    FetchedLines,
    Liveness,
    Search,
}

impl RpcConsumerKind {
    fn metric_label(self) -> &'static str {
        match self {
            Self::TopologySnapshot => "topology_snapshot",
            Self::RenderBootstrap => "render_bootstrap",
            Self::InitialAttachment => "initial_attachment",
            Self::InitialAttachmentCleanup => "initial_attachment_cleanup",
            Self::SpawnResolution => "spawn_resolution",
            Self::MoveResolution => "move_resolution",
            Self::SplitResolution => "split_resolution",
            Self::GlobalUnilateral => "global_unilateral",
            Self::PaneUnilateral => "pane_unilateral",
            Self::FetchedLines => "fetched_lines",
            Self::Liveness => "liveness",
            Self::Search => "search",
        }
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub(crate) enum RpcConsumerCommitError {
    #[error("mux RPC {consumer:?} consumer has no exact transport generation")]
    Unavailable { consumer: RpcConsumerKind },
    #[error(
        "mux RPC {consumer:?} consumer for generation {bound_generation} was rejected; \
         active generation is {active_generation:?}"
    )]
    Retired {
        consumer: RpcConsumerKind,
        bound_generation: NonZeroU64,
        active_generation: Option<NonZeroU64>,
    },
    #[error(
        "mux RPC {consumer:?} consumer-commit accounting overflowed in generation {generation}"
    )]
    AccountingOverflow {
        consumer: RpcConsumerKind,
        generation: NonZeroU64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcTransportPhase {
    Live(NonZeroU64),
    Reconnecting {
        retired: NonZeroU64,
        next: NonZeroU64,
    },
    Closed {
        last_live: NonZeroU64,
    },
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("mux RPC generation {generation} was cancelled while reader I/O was pending: {reason}")]
struct RpcGenerationReaderAborted {
    generation: NonZeroU64,
    reason: &'static str,
}

/// Sticky, generation-scoped cancellation authority for the one physical
/// reader that owns socket I/O.
///
/// The ordinary request queue is not a cancellation primitive: a reader can be
/// suspended inside a write, flush, or partial-frame decode and never poll that
/// queue again. A single-reader atomic waker interrupts those operations
/// without channel traffic on every I/O turn, while the sticky cause closes
/// the race where cancellation commits immediately before or after an I/O
/// future becomes ready. A successor generation receives a fresh authority,
/// so a stale wake can never cancel its transport.
#[derive(Debug)]
struct RpcGenerationReaderAbortAuthority {
    generation: NonZeroU64,
    cancelled: AtomicBool,
    cause: ParkingMutex<Option<&'static str>>,
    reader_waker: futures::task::AtomicWaker,
}

impl RpcGenerationReaderAbortAuthority {
    fn new(generation: NonZeroU64) -> Self {
        Self {
            generation,
            cancelled: AtomicBool::new(false),
            cause: ParkingMutex::new(None),
            reader_waker: futures::task::AtomicWaker::new(),
        }
    }

    fn cause(&self) -> Option<&'static str> {
        if self.cancelled.load(AtomicOrdering::Acquire) {
            *self.cause.lock()
        } else {
            None
        }
    }

    fn commit_abort(&self, reason: &'static str) -> bool {
        let first = {
            let mut cause = self.cause.lock();
            if cause.is_some() {
                false
            } else {
                *cause = Some(reason);
                true
            }
        };
        if first {
            self.cancelled.store(true, AtomicOrdering::Release);
        }
        first
    }

    fn wake_reader(&self) {
        self.reader_waker.wake();
    }

    async fn cancelled(&self) {
        poll_fn(|task_cx| {
            if self.cancelled.load(AtomicOrdering::Acquire) {
                return Poll::Ready(());
            }
            self.reader_waker.register(task_cx.waker());
            if self.cancelled.load(AtomicOrdering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    fn aborted_error(&self) -> Option<anyhow::Error> {
        self.cause().map(|reason| {
            anyhow::Error::new(RpcGenerationReaderAborted {
                generation: self.generation,
                reason,
            })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RpcCodecAuthority {
    generation: NonZeroU64,
    local_max: usize,
    local_min: usize,
    remote_max: usize,
    remote_min: usize,
    agreed: usize,
    dialect: MuxWireDialect,
}

impl RpcCodecAuthority {
    fn negotiate(
        generation: NonZeroU64,
        remote_max: usize,
        advertised_remote_min: usize,
    ) -> Result<Self, codec::CompatError> {
        let remote_min = if advertised_remote_min == 0 {
            remote_max
        } else {
            advertised_remote_min
        };
        let (agreed, dialect) =
            if remote_max == LEGACY46_CODEC_VERSION && remote_min == LEGACY46_CODEC_VERSION {
                (LEGACY46_CODEC_VERSION, MuxWireDialect::LEGACY46)
            } else {
                let codec::CompatDecision::Compatible { agreed } = codec::check_compat(
                    CODEC_VERSION,
                    codec::CODEC_VERSION_MIN_SUPPORTED,
                    remote_max,
                    remote_min,
                )?;
                let dialect = MuxWireDialect::current(agreed)
                    .expect("a negotiated current compatibility window yields a closed dialect");
                (agreed, dialect)
            };
        Ok(Self {
            generation,
            local_max: CODEC_VERSION,
            local_min: codec::CODEC_VERSION_MIN_SUPPORTED,
            remote_max,
            remote_min,
            agreed,
            dialect,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcProtocolPhase {
    AwaitingCodecRequest,
    AwaitingCodecResponse,
    AwaitingRegistrationRequest,
    AwaitingRegistrationResponse,
    Established,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcProtocolDirection {
    Outbound,
    Inbound,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum OrdinaryMuxProtocolError {
    #[error("ordinary mux {direction:?} PDU identity {ident} has no assigned wire policy")]
    UnknownPdu {
        direction: RpcProtocolDirection,
        ident: u64,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) is not legal during protocol phase \
         {phase:?}"
    )]
    PhaseViolation {
        direction: RpcProtocolDirection,
        ident: u64,
        name: &'static str,
        phase: RpcProtocolPhase,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) does not authorize producer \
         {producer:?} with role {role:?}"
    )]
    DirectionViolation {
        direction: RpcProtocolDirection,
        ident: u64,
        name: &'static str,
        producer: PduProducer,
        role: PduWireRole,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) requires codec dialect \
         {required}, but generation {generation} agreed on {agreed}"
    )]
    DialectViolation {
        direction: RpcProtocolDirection,
        generation: NonZeroU64,
        ident: u64,
        name: &'static str,
        required: usize,
        agreed: usize,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) requires locally activated \
         capabilities {required:#x}, but this client activates {activated:#x}"
    )]
    CapabilityNotActivated {
        direction: RpcProtocolDirection,
        ident: u64,
        name: &'static str,
        required: u64,
        activated: u64,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) requires established capabilities \
         {required:#x}, but generation {generation} established {established:#x}"
    )]
    CapabilityNotEstablished {
        direction: RpcProtocolDirection,
        generation: NonZeroU64,
        ident: u64,
        name: &'static str,
        required: u64,
        established: u64,
    },
    #[error(
        "ordinary mux outbound ListPanesCoherent advertised supported={supported:#x}, \
         required={required:#x}; the exact activated mask is {activated:#x}"
    )]
    CapabilityAdvertisementMismatch {
        supported: u64,
        required: u64,
        activated: u64,
    },
    #[error(
        "ordinary mux {direction:?} PDU {name} ({ident}) belongs to an inactive endpoint family"
    )]
    EndpointInactive {
        direction: RpcProtocolDirection,
        ident: u64,
        name: &'static str,
    },
    #[error(
        "ordinary mux codec authority belongs to generation {authority_generation}, not \
         requested generation {requested_generation}"
    )]
    CodecGenerationMismatch {
        authority_generation: NonZeroU64,
        requested_generation: NonZeroU64,
    },
    #[error("ordinary mux generation {generation} has no live protocol authority")]
    ProtocolAuthorityUnavailable { generation: NonZeroU64 },
}

impl OrdinaryMuxProtocolError {
    const fn metric_reason(&self) -> &'static str {
        match self {
            Self::UnknownPdu { .. } => "unknown_pdu",
            Self::PhaseViolation { .. } => "phase_violation",
            Self::DirectionViolation { .. } => "direction_violation",
            Self::DialectViolation { .. } => "dialect_violation",
            Self::CapabilityNotActivated { .. } => "capability_not_activated",
            Self::CapabilityNotEstablished { .. } => "capability_not_established",
            Self::CapabilityAdvertisementMismatch { .. } => "capability_advertisement_mismatch",
            Self::EndpointInactive { .. } => "endpoint_inactive",
            Self::CodecGenerationMismatch { .. } => "codec_generation_mismatch",
            Self::ProtocolAuthorityUnavailable { .. } => "protocol_authority_unavailable",
        }
    }
}

/// Classify the one local PDU98 dialect rejection that is known to have
/// crossed no wire boundary and can safely restore a queued sampled context.
///
/// A reliable-input claim can observe v62 immediately before a reconnect, then
/// construct its typed RPC against the fresh v61 generation. Every outbound
/// dialect validation cut precedes frame encoding and physical write, so this
/// exact error is `DefinitelyNotSent`. Restrict the classification to a
/// dialect that still supports byte-identical PDU96; an actual reliable-input
/// downgrade remains terminal.
pub(crate) fn is_definitely_not_sent_reliable_trace_dialect_rejection(
    error: &anyhow::Error,
) -> bool {
    matches!(
        error
            .root_cause()
            .downcast_ref::<OrdinaryMuxProtocolError>(),
        Some(OrdinaryMuxProtocolError::DialectViolation {
            direction: RpcProtocolDirection::Outbound,
            ident,
            name,
            required,
            agreed,
            ..
        }) if *ident == <ReliableKeyEventTracedV1 as PduWireIdent>::IDENT
            && *name == <ReliableKeyEventTracedV1 as PduWireIdent>::WIRE_SPEC.name
            && *required == RELIABLE_KEY_EVENT_TRACED_V1_MIN_CODEC_VERSION
            && *agreed >= RELIABLE_KEY_EVENT_V1_MIN_CODEC_VERSION
            && *agreed < *required
    )
}

/// Classify a local PDU100 dialect race before framing or physical write.
///
/// A queued pane write may bind to v64 immediately before reconnect installs
/// an older generation. The old peer is not a compatibility target for this
/// operation, but this exact rejection proves that the original identity and
/// bytes did not cross a wire boundary; the queue can therefore re-evaluate
/// the successor authority without marking the write ambiguous.
pub(crate) fn is_definitely_not_sent_reliable_pane_write_dialect_rejection(
    error: &anyhow::Error,
) -> bool {
    matches!(
        error
            .root_cause()
            .downcast_ref::<OrdinaryMuxProtocolError>(),
        Some(OrdinaryMuxProtocolError::DialectViolation {
            direction: RpcProtocolDirection::Outbound,
            ident,
            name,
            required,
            agreed,
            ..
        }) if *ident == <ReliablePaneWriteV1 as PduWireIdent>::IDENT
            && *name == <ReliablePaneWriteV1 as PduWireIdent>::WIRE_SPEC.name
            && *required == RELIABLE_PANE_WRITE_V1_MIN_CODEC_VERSION
            && *agreed < *required
    )
}

fn record_ordinary_mux_protocol_rejection(
    error: &OrdinaryMuxProtocolError,
    direction: &'static str,
    stage: &'static str,
) {
    metrics::counter!(
        "mux.client.protocol.rejection.total",
        "direction" => direction,
        "stage" => stage,
        "reason" => error.metric_reason(),
    )
    .increment(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcOutboundAdmissionPoint {
    Preflight,
    Enqueue,
    Dequeue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcProtocolTransition {
    None,
    CodecRequest,
    RegistrationRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RpcProtocolAuthority {
    generation: NonZeroU64,
    phase: RpcProtocolPhase,
    codec: Option<RpcCodecAuthority>,
    established_capabilities: TopologyCapabilities,
}

impl RpcProtocolAuthority {
    fn new(generation: NonZeroU64) -> Self {
        Self {
            generation,
            phase: RpcProtocolPhase::AwaitingCodecRequest,
            codec: None,
            established_capabilities: TopologyCapabilities::NONE,
        }
    }

    #[cfg(test)]
    fn established_for_test(generation: NonZeroU64, agreed: usize) -> Self {
        let dialect = if agreed == LEGACY46_CODEC_VERSION {
            MuxWireDialect::LEGACY46
        } else {
            MuxWireDialect::current(agreed)
                .expect("test protocol authority requires a supported exact dialect")
        };
        Self {
            generation,
            phase: RpcProtocolPhase::Established,
            codec: Some(RpcCodecAuthority {
                generation,
                local_max: CODEC_VERSION,
                local_min: codec::CODEC_VERSION_MIN_SUPPORTED,
                remote_max: agreed,
                remote_min: agreed,
                agreed,
                dialect,
            }),
            established_capabilities: TopologyCapabilities::NONE,
        }
    }

    const fn locally_activated_capabilities() -> TopologyCapabilities {
        // Every additive capability remains deliberately inactive. Do not
        // replace this exact mask with decoder support or a broad server mask.
        TopologyCapabilities::FENCED_SNAPSHOT_V1
    }

    fn endpoint_is_activated(spec: &PduWireSpec) -> bool {
        // Fail closed for every additive endpoint until the ordinary client
        // has an explicit live coordinator. Decoder/handler presence is not
        // activation. Spell the assigned legacy ranges exactly: a future PDU
        // placed in one of the historical numeric gaps must remain dormant by
        // default instead of inheriting authority from a broad `<=` cutoff.
        // Above the legacy surface, only the fenced-topology trio, the
        // explicitly coordinated reliable-input request/reply and sampled
        // paste request, and the reliable-pane-write request/reply are active.
        matches!(
            spec.ident,
            0..=4 | 8..=14 | 20 | 22..=78 | 81..=83 | 96..=101
        )
    }

    fn uses_ordered_window_capability(spec: &PduWireSpec) -> bool {
        let capability = match spec.capability {
            PduCapabilityUse::Negotiates(capability) | PduCapabilityUse::Requires(capability) => {
                capability
            }
            PduCapabilityUse::None => return false,
        };
        capability.contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1)
    }

    fn agreed_dialect(&self) -> usize {
        self.codec.map_or(CODEC_VERSION, |codec| codec.agreed)
    }

    fn wire_dialect(&self) -> MuxWireDialect {
        self.codec.map_or_else(
            || {
                MuxWireDialect::current(CODEC_VERSION)
                    .expect("the bootstrap request uses this build's current dialect")
            },
            |codec| codec.dialect,
        )
    }

    fn validate_common(
        &self,
        spec: &PduWireSpec,
        direction: RpcProtocolDirection,
        producer: PduProducer,
        role: PduWireRole,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        if !Self::endpoint_is_activated(spec) {
            return Err(OrdinaryMuxProtocolError::EndpointInactive {
                direction,
                ident: spec.ident,
                name: spec.name,
            });
        }
        if !spec.authorizes(producer, role) {
            return Err(OrdinaryMuxProtocolError::DirectionViolation {
                direction,
                ident: spec.ident,
                name: spec.name,
                producer,
                role,
            });
        }
        let agreed = self.agreed_dialect();
        if !self.wire_dialect().admits_wire_spec(spec) {
            return Err(OrdinaryMuxProtocolError::DialectViolation {
                direction,
                generation: self.generation,
                ident: spec.ident,
                name: spec.name,
                required: spec.min_codec_version,
                agreed,
            });
        }
        match spec.capability {
            PduCapabilityUse::None => {}
            PduCapabilityUse::Negotiates(required) => {
                let activated = Self::locally_activated_capabilities();
                if !activated.contains(required) {
                    return Err(OrdinaryMuxProtocolError::CapabilityNotActivated {
                        direction,
                        ident: spec.ident,
                        name: spec.name,
                        required: required.bits(),
                        activated: activated.bits(),
                    });
                }
            }
            PduCapabilityUse::Requires(required) => {
                let activated = Self::locally_activated_capabilities();
                if !activated.contains(required) {
                    return Err(OrdinaryMuxProtocolError::CapabilityNotActivated {
                        direction,
                        ident: spec.ident,
                        name: spec.name,
                        required: required.bits(),
                        activated: activated.bits(),
                    });
                }
                if !self.established_capabilities.contains(required) {
                    return Err(OrdinaryMuxProtocolError::CapabilityNotEstablished {
                        direction,
                        generation: self.generation,
                        ident: spec.ident,
                        name: spec.name,
                        required: required.bits(),
                        established: self.established_capabilities.bits(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_outbound(
        &self,
        spec: &PduWireSpec,
        point: RpcOutboundAdmissionPoint,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.validate_common(
            spec,
            RpcProtocolDirection::Outbound,
            PduProducer::Client,
            PduWireRole::Request,
        )?;
        let codec_request = spec.ident == <GetCodecVersion as PduWireIdent>::IDENT;
        let registration_request = spec.ident == <SetClientId as PduWireIdent>::IDENT;
        let phase_authorized = match (self.phase, point) {
            (
                RpcProtocolPhase::AwaitingCodecRequest,
                RpcOutboundAdmissionPoint::Preflight | RpcOutboundAdmissionPoint::Enqueue,
            ) => codec_request,
            (RpcProtocolPhase::AwaitingCodecResponse, RpcOutboundAdmissionPoint::Dequeue) => {
                codec_request
            }
            (
                RpcProtocolPhase::AwaitingRegistrationRequest,
                RpcOutboundAdmissionPoint::Preflight | RpcOutboundAdmissionPoint::Enqueue,
            ) => registration_request,
            (
                RpcProtocolPhase::AwaitingRegistrationResponse,
                RpcOutboundAdmissionPoint::Dequeue,
            ) => registration_request,
            (RpcProtocolPhase::Established, _) => !codec_request && !registration_request,
            _ => false,
        };
        if !phase_authorized {
            return Err(OrdinaryMuxProtocolError::PhaseViolation {
                direction: RpcProtocolDirection::Outbound,
                ident: spec.ident,
                name: spec.name,
                phase: self.phase,
            });
        }
        Ok(())
    }

    fn validate_outbound_pdu(
        &self,
        pdu: &Pdu,
        point: RpcOutboundAdmissionPoint,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        let spec = assigned_pdu_spec(pdu, RpcProtocolDirection::Outbound)?;
        self.validate_outbound(spec, point)?;
        if let Pdu::ListPanesCoherent(request) = pdu {
            let activated = Self::locally_activated_capabilities();
            if request.supported != activated || request.required != activated {
                return Err(OrdinaryMuxProtocolError::CapabilityAdvertisementMismatch {
                    supported: request.supported.bits(),
                    required: request.required.bits(),
                    activated: activated.bits(),
                });
            }
        }
        Ok(())
    }

    fn admit_outbound(
        &mut self,
        pdu: &Pdu,
    ) -> Result<RpcProtocolTransition, OrdinaryMuxProtocolError> {
        self.validate_outbound_pdu(pdu, RpcOutboundAdmissionPoint::Enqueue)?;
        let transition = match self.phase {
            RpcProtocolPhase::AwaitingCodecRequest => {
                self.phase = RpcProtocolPhase::AwaitingCodecResponse;
                RpcProtocolTransition::CodecRequest
            }
            RpcProtocolPhase::AwaitingRegistrationRequest => {
                self.phase = RpcProtocolPhase::AwaitingRegistrationResponse;
                RpcProtocolTransition::RegistrationRequest
            }
            RpcProtocolPhase::Established => RpcProtocolTransition::None,
            _ => unreachable!("outbound admission validated an ineligible protocol phase"),
        };
        Ok(transition)
    }

    fn rollback_outbound(&mut self, transition: RpcProtocolTransition) {
        self.phase = match (transition, self.phase) {
            (RpcProtocolTransition::None, phase) => phase,
            (RpcProtocolTransition::CodecRequest, RpcProtocolPhase::AwaitingCodecResponse) => {
                RpcProtocolPhase::AwaitingCodecRequest
            }
            (
                RpcProtocolTransition::RegistrationRequest,
                RpcProtocolPhase::AwaitingRegistrationResponse,
            ) => RpcProtocolPhase::AwaitingRegistrationRequest,
            _ => self.phase,
        };
    }

    fn rollback_unadmitted_outbound(&mut self, pdu: &Pdu) -> Result<(), OrdinaryMuxProtocolError> {
        let spec = assigned_pdu_spec(pdu, RpcProtocolDirection::Outbound)?;
        self.rollback_unadmitted_outbound_ident(spec.ident);
        Ok(())
    }

    fn rollback_unadmitted_outbound_ident(&mut self, ident: u64) {
        let transition = if ident == <GetCodecVersion as PduWireIdent>::IDENT {
            RpcProtocolTransition::CodecRequest
        } else if ident == <SetClientId as PduWireIdent>::IDENT {
            RpcProtocolTransition::RegistrationRequest
        } else {
            RpcProtocolTransition::None
        };
        self.rollback_outbound(transition);
    }

    fn validate_inbound(
        &self,
        spec: &PduWireSpec,
        role: PduWireRole,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.validate_common(
            spec,
            RpcProtocolDirection::Inbound,
            PduProducer::Server,
            role,
        )?;
        let codec_response = spec.ident == <GetCodecVersionResponse as PduWireIdent>::IDENT;
        let unit_response = spec.ident == <UnitResponse as PduWireIdent>::IDENT;
        let error_response = spec.ident == <ErrorResponse as PduWireIdent>::IDENT;
        let phase_authorized = match self.phase {
            RpcProtocolPhase::AwaitingCodecResponse => codec_response || error_response,
            RpcProtocolPhase::AwaitingRegistrationResponse => unit_response || error_response,
            RpcProtocolPhase::Established => !codec_response,
            RpcProtocolPhase::AwaitingCodecRequest
            | RpcProtocolPhase::AwaitingRegistrationRequest => false,
        };
        if !phase_authorized {
            return Err(OrdinaryMuxProtocolError::PhaseViolation {
                direction: RpcProtocolDirection::Inbound,
                ident: spec.ident,
                name: spec.name,
                phase: self.phase,
            });
        }
        Ok(())
    }

    fn install_codec(&mut self, codec: RpcCodecAuthority) -> Result<(), OrdinaryMuxProtocolError> {
        if codec.generation != self.generation {
            return Err(OrdinaryMuxProtocolError::CodecGenerationMismatch {
                authority_generation: codec.generation,
                requested_generation: self.generation,
            });
        }
        if self.phase == RpcProtocolPhase::AwaitingRegistrationRequest && self.codec == Some(codec)
        {
            return Ok(());
        }
        if self.phase != RpcProtocolPhase::AwaitingCodecResponse {
            return Err(OrdinaryMuxProtocolError::PhaseViolation {
                direction: RpcProtocolDirection::Inbound,
                ident: <GetCodecVersionResponse as PduWireIdent>::IDENT,
                name: "GetCodecVersionResponse",
                phase: self.phase,
            });
        }
        self.codec = Some(codec);
        self.phase = RpcProtocolPhase::AwaitingRegistrationRequest;
        Ok(())
    }

    fn complete_correlated_response(
        &mut self,
        request: &'static str,
        pdu: &Pdu,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        if request == "GetCodecVersion" {
            if let Pdu::GetCodecVersionResponse(info) = pdu {
                // Install a compatible window in the reader before it can
                // select another frame. The caller repeats the same exact
                // installation after applying UI/topology policy; that path
                // is intentionally idempotent. An incompatible tuple remains
                // caller-visible through the existing version error flow.
                if let Ok(codec) = RpcCodecAuthority::negotiate(
                    self.generation,
                    info.codec_vers,
                    info.min_supported,
                ) {
                    self.install_codec(codec)?;
                }
            }
        }
        self.complete_registration(request, pdu)
    }

    fn complete_registration(
        &mut self,
        request: &'static str,
        pdu: &Pdu,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        if request != "SetClientId" || !matches!(pdu, Pdu::UnitResponse(_)) {
            return Ok(());
        }
        if self.phase != RpcProtocolPhase::AwaitingRegistrationResponse {
            let spec = pdu
                .wire_spec()
                .expect("UnitResponse always has generated wire policy");
            return Err(OrdinaryMuxProtocolError::PhaseViolation {
                direction: RpcProtocolDirection::Inbound,
                ident: spec.ident,
                name: spec.name,
                phase: self.phase,
            });
        }
        self.phase = RpcProtocolPhase::Established;
        Ok(())
    }

    fn establish_capabilities(
        &mut self,
        capabilities: TopologyCapabilities,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        if self.phase != RpcProtocolPhase::Established {
            return Err(OrdinaryMuxProtocolError::PhaseViolation {
                direction: RpcProtocolDirection::Inbound,
                ident: <ListPanesCoherentResponse as PduWireIdent>::IDENT,
                name: "ListPanesCoherentResponse",
                phase: self.phase,
            });
        }
        let activated = Self::locally_activated_capabilities();
        if !activated.contains(capabilities) {
            return Err(OrdinaryMuxProtocolError::CapabilityNotActivated {
                direction: RpcProtocolDirection::Inbound,
                ident: <ListPanesCoherentResponse as PduWireIdent>::IDENT,
                name: "ListPanesCoherentResponse",
                required: capabilities.bits(),
                activated: activated.bits(),
            });
        }
        self.established_capabilities = TopologyCapabilities::from_bits(
            self.established_capabilities.bits() | capabilities.bits(),
        );
        Ok(())
    }
}

fn assigned_pdu_spec(
    pdu: &Pdu,
    direction: RpcProtocolDirection,
) -> Result<&'static PduWireSpec, OrdinaryMuxProtocolError> {
    pdu.wire_spec().ok_or_else(|| {
        let ident = match pdu {
            Pdu::Invalid { ident } => *ident,
            _ => unreachable!("assigned PDU variant is missing generated wire policy"),
        };
        OrdinaryMuxProtocolError::UnknownPdu { direction, ident }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcReadinessAuthorityPhase {
    Pending,
    Ready,
    Retired,
    AbortCommitted,
}

#[derive(Debug)]
struct RpcReadinessAuthorityState {
    participants: usize,
    queued_publications: usize,
    phase: RpcReadinessAuthorityPhase,
}

#[derive(Debug)]
struct RpcReadinessAuthority {
    generation: NonZeroU64,
    state: ParkingMutex<RpcReadinessAuthorityState>,
}

impl RpcReadinessAuthority {
    fn new(generation: NonZeroU64) -> Self {
        Self {
            generation,
            state: ParkingMutex::new(RpcReadinessAuthorityState {
                participants: 0,
                queued_publications: 0,
                phase: RpcReadinessAuthorityPhase::Pending,
            }),
        }
    }

    fn register_participant(&self) -> anyhow::Result<bool> {
        let mut state = self.state.lock();
        match state.phase {
            RpcReadinessAuthorityPhase::Pending => {
                if state.participants >= MAX_RPC_READINESS_PARTICIPANTS {
                    metrics::counter!(
                        "mux.client.rpc.readiness_participant.total",
                        "outcome" => "limit_rejected"
                    )
                    .increment(1);
                    bail!(
                        "mux RPC generation {} reached its {} readiness-participant limit",
                        self.generation,
                        MAX_RPC_READINESS_PARTICIPANTS
                    );
                }
                state.participants = state
                    .participants
                    .checked_add(1)
                    .context("mux RPC readiness-participant count overflow")?;
                drop(state);
                metrics::counter!(
                    "mux.client.rpc.readiness_participant.total",
                    "outcome" => "registered"
                )
                .increment(1);
                Ok(true)
            }
            RpcReadinessAuthorityPhase::Ready => Ok(false),
            RpcReadinessAuthorityPhase::Retired => {
                bail!(
                    "mux RPC generation {} retired before readiness participation",
                    self.generation
                )
            }
            RpcReadinessAuthorityPhase::AbortCommitted => {
                bail!(
                    "mux RPC generation {} already committed readiness abort",
                    self.generation
                )
            }
        }
    }

    fn release_participant(&self, armed: bool) -> bool {
        let mut state = self.state.lock();
        state.participants = state
            .participants
            .checked_sub(1)
            .expect("mux RPC readiness-participant accounting underflow");
        let phase = state.phase;
        let should_abort =
            armed && state.participants == 0 && phase == RpcReadinessAuthorityPhase::Pending;
        if should_abort {
            state.phase = RpcReadinessAuthorityPhase::AbortCommitted;
        }
        let outcome = if should_abort {
            "last_cancelled"
        } else if armed && phase == RpcReadinessAuthorityPhase::Pending {
            "cancelled_handoff"
        } else if armed && phase == RpcReadinessAuthorityPhase::Ready {
            "cancelled_after_ready"
        } else if armed && phase == RpcReadinessAuthorityPhase::AbortCommitted {
            "abort_already_committed"
        } else if armed {
            "retired"
        } else {
            "completed"
        };
        drop(state);
        metrics::counter!(
            "mux.client.rpc.readiness_participant.total",
            "outcome" => outcome
        )
        .increment(1);
        should_abort
    }

    fn mark_ready(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        match state.phase {
            RpcReadinessAuthorityPhase::Pending => {
                if state.participants == 0 {
                    bail!(
                        "mux RPC generation {} has no live readiness participant at commit",
                        self.generation
                    );
                }
                state.phase = RpcReadinessAuthorityPhase::Ready;
                Ok(())
            }
            RpcReadinessAuthorityPhase::Ready => Ok(()),
            RpcReadinessAuthorityPhase::Retired => {
                bail!(
                    "mux RPC generation {} retired before readiness commit",
                    self.generation
                )
            }
            RpcReadinessAuthorityPhase::AbortCommitted => {
                bail!(
                    "mux RPC generation {} lost all readiness participants before commit",
                    self.generation
                )
            }
        }
    }

    fn commit_fatal_abort(&self) -> bool {
        let committed = {
            let mut state = self.state.lock();
            if state.phase != RpcReadinessAuthorityPhase::Pending {
                false
            } else {
                state.phase = RpcReadinessAuthorityPhase::AbortCommitted;
                true
            }
        };
        if committed {
            metrics::counter!(
                "mux.client.rpc.readiness_abort.total",
                "cause" => "fatal_replay"
            )
            .increment(1);
        }
        committed
    }

    fn retire(&self) {
        let mut state = self.state.lock();
        // A fatal replay guard commits stronger, causal terminal evidence than
        // ordinary transport retirement.  Preserve it when the same abort
        // revokes admission; otherwise the abort path immediately erases the
        // state that prevents participant resurrection and classifies later
        // guard settlement.
        if state.phase != RpcReadinessAuthorityPhase::AbortCommitted {
            state.phase = RpcReadinessAuthorityPhase::Retired;
        }
    }

    fn reserve_publication(self: &Arc<Self>) -> anyhow::Result<RpcReadinessPublicationLease> {
        let mut state = self.state.lock();
        if matches!(
            state.phase,
            RpcReadinessAuthorityPhase::Retired | RpcReadinessAuthorityPhase::AbortCommitted
        ) {
            bail!(
                "mux RPC generation {} cannot accept a readiness publication in phase {:?}",
                self.generation,
                state.phase
            );
        }
        if state.queued_publications >= MAX_RPC_READINESS_PUBLICATIONS {
            metrics::counter!(
                "mux.client.rpc.readiness_publication.total",
                "outcome" => "limit_rejected"
            )
            .increment(1);
            bail!(
                "mux RPC generation {} reached its {} readiness-publication limit",
                self.generation,
                MAX_RPC_READINESS_PUBLICATIONS
            );
        }
        state.queued_publications = state
            .queued_publications
            .checked_add(1)
            .context("mux RPC readiness-publication count overflow")?;
        drop(state);
        metrics::counter!(
            "mux.client.rpc.readiness_publication.total",
            "outcome" => "reserved"
        )
        .increment(1);
        Ok(RpcReadinessPublicationLease {
            authority: Arc::clone(self),
        })
    }

    fn release_publication(&self) {
        let mut state = self.state.lock();
        state.queued_publications = state
            .queued_publications
            .checked_sub(1)
            .expect("mux RPC readiness-publication accounting underflow");
        drop(state);
        metrics::counter!(
            "mux.client.rpc.readiness_publication.total",
            "outcome" => "released"
        )
        .increment(1);
    }
}

struct RpcReadinessPublicationLease {
    authority: Arc<RpcReadinessAuthority>,
}

impl Drop for RpcReadinessPublicationLease {
    fn drop(&mut self) {
        self.authority.release_publication();
    }
}

#[derive(Debug)]
struct RpcTransportLifecycle {
    phase: RpcTransportPhase,
    /// Exact-generation bootstrap, codec-dialect, direction, and capability
    /// authority. `None` means that no live transport owns wire authority.
    protocol: Option<RpcProtocolAuthority>,
    active_consumer_commits: usize,
    terminal_error: Option<RpcTransportError>,
    readiness_authority: Arc<RpcReadinessAuthority>,
    /// Exact-generation sticky cancellation and wake authority for the socket
    /// reader. Replaced before each successor generation becomes live.
    reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    /// Exact stream/session authority established only after the coherent
    /// topology snapshot has been applied and committed by its consumer.
    render_connection_identity: Option<RenderConnectionIdentity>,
}

impl RpcTransportLifecycle {
    fn protocol_for(
        &self,
        generation: NonZeroU64,
    ) -> Result<&RpcProtocolAuthority, OrdinaryMuxProtocolError> {
        let protocol = self
            .protocol
            .as_ref()
            .ok_or(OrdinaryMuxProtocolError::ProtocolAuthorityUnavailable { generation })?;
        if protocol.generation != generation {
            return Err(OrdinaryMuxProtocolError::CodecGenerationMismatch {
                authority_generation: protocol.generation,
                requested_generation: generation,
            });
        }
        Ok(protocol)
    }

    fn protocol_for_mut(
        &mut self,
        generation: NonZeroU64,
    ) -> Result<&mut RpcProtocolAuthority, OrdinaryMuxProtocolError> {
        let protocol = self
            .protocol
            .as_mut()
            .ok_or(OrdinaryMuxProtocolError::ProtocolAuthorityUnavailable { generation })?;
        if protocol.generation != generation {
            return Err(OrdinaryMuxProtocolError::CodecGenerationMismatch {
                authority_generation: protocol.generation,
                requested_generation: generation,
            });
        }
        Ok(protocol)
    }
}

#[derive(Debug)]
struct RpcTransportState {
    lifecycle: ParkingMutex<RpcTransportLifecycle>,
    consumer_commits_drained: Condvar,
    /// One incarnation-wide root bounds every queued generation, including
    /// old work that has not yet observed retirement and a newly activated
    /// successor. Leases themselves remain bound to one exact generation.
    outbound_budget: Arc<ClientOutboundBudget>,
    /// Hot-path mirror. Zero means that external RPC admission is disabled.
    live_generation: AtomicU64,
    /// Ambient user/workflow admission is enabled only after this physical
    /// transport completes its exact-generation codec and identity handshake.
    ready_generation: AtomicU64,
    next_attempt_id: AtomicU64,
    /// Wire serials are never reused during one Client incarnation. This makes
    /// a stale response from an old physical stream unmatched on its successor
    /// even though the existing codec does not carry a generation nonce.
    next_wire_serial: AtomicU64,
    /// Dedicated one-shot wake that is raced against every potentially
    /// blocking reader I/O operation. The ordinary request queue cannot
    /// interrupt an already-polled write, flush, or partial-frame decode.
    terminal_reader_wake_tx: Sender<()>,
    terminal_reader_wake_rx: Receiver<()>,
    /// Serialize coherent topology snapshots through consumer commit. The
    /// server deliberately permits only one active causal fence per
    /// connection, and releasing this gate before the reader acknowledges the
    /// exact snapshot would let concurrent resyncs destroy that invariant.
    topology_sync: futures::lock::Mutex<()>,
}

struct RpcGenerationCommitLease {
    rpc_transport: Arc<RpcTransportState>,
    consumer: RpcConsumerKind,
}

impl Drop for RpcGenerationCommitLease {
    fn drop(&mut self) {
        let commits_drained = {
            let mut lifecycle = self.rpc_transport.lifecycle.lock();
            lifecycle.active_consumer_commits = lifecycle
                .active_consumer_commits
                .checked_sub(1)
                .expect("mux RPC consumer-commit lease accounting underflow");
            lifecycle.active_consumer_commits == 0
        };
        if commits_drained {
            self.rpc_transport.consumer_commits_drained.notify_all();
        }
        metrics::counter!(
            "mux.client.rpc.consumer_commit.total",
            "consumer" => self.consumer.metric_label(),
            "outcome" => "completed"
        )
        .increment(1);
    }
}

impl RpcTransportState {
    fn new() -> Self {
        let generation = NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
            .expect("the initial connection generation is nonzero");
        let (terminal_reader_wake_tx, terminal_reader_wake_rx) = bounded(1);
        Self {
            lifecycle: ParkingMutex::new(RpcTransportLifecycle {
                phase: RpcTransportPhase::Live(generation),
                protocol: Some(RpcProtocolAuthority::new(generation)),
                active_consumer_commits: 0,
                terminal_error: None,
                readiness_authority: Arc::new(RpcReadinessAuthority::new(generation)),
                reader_abort: Arc::new(RpcGenerationReaderAbortAuthority::new(generation)),
                render_connection_identity: None,
            }),
            consumer_commits_drained: Condvar::new(),
            outbound_budget: Arc::new(ClientOutboundBudget::default()),
            live_generation: AtomicU64::new(generation.get()),
            ready_generation: AtomicU64::new(0),
            next_attempt_id: AtomicU64::new(1),
            next_wire_serial: AtomicU64::new(1),
            terminal_reader_wake_tx,
            terminal_reader_wake_rx,
            topology_sync: futures::lock::Mutex::new(()),
        }
    }

    #[cfg(test)]
    fn new_with_outbound_budget_limits(limits: ClientOutboundBudgetLimits) -> Self {
        let mut state = Self::new();
        state.outbound_budget = Arc::new(ClientOutboundBudget::with_limits(limits));
        state
    }

    fn allocate_monotonic(counter: &AtomicU64) -> Result<NonZeroU64, u64> {
        let mut current = counter.load(AtomicOrdering::Acquire);
        loop {
            let Some(allocated) = NonZeroU64::new(current) else {
                return Err(current);
            };
            let next = current.checked_add(1).unwrap_or(0);
            match counter.compare_exchange(
                current,
                next,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return Ok(allocated),
                Err(observed) => current = observed,
            }
        }
    }

    fn allocate_attempt(&self, request: &'static str) -> Result<NonZeroU64, RpcTransportError> {
        if let Some(error) = self.lifecycle.lock().terminal_error.clone() {
            record_rpc_transport_error(&error);
            return Err(error);
        }
        Self::allocate_monotonic(&self.next_attempt_id).map_err(|_| {
            let error = self
                .mark_incarnation_terminal(RpcTransportError::AttemptIdentityExhausted { request });
            record_rpc_transport_error(&error);
            error
        })
    }

    fn allocate_wire_serial(&self) -> Result<NonZeroU64, PendingRpcError> {
        Self::allocate_monotonic(&self.next_wire_serial)
            .map_err(|_| PendingRpcError::SerialExhausted)
    }

    fn active_generation(&self) -> Option<NonZeroU64> {
        NonZeroU64::new(self.live_generation.load(AtomicOrdering::Acquire))
    }

    fn bind_render_connection_identity(
        &self,
        generation: NonZeroU64,
        identity: RenderConnectionIdentity,
    ) -> anyhow::Result<()> {
        identity
            .validate()
            .context("validating render connection identity")?;
        let mut lifecycle = self.lifecycle.lock();
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) || self.live_generation.load(AtomicOrdering::Acquire) != generation.get()
        {
            bail!(
                "cannot bind render connection identity for retired mux RPC generation {}",
                generation
            );
        }
        match lifecycle.render_connection_identity {
            None => {
                lifecycle.render_connection_identity = Some(identity);
                Ok(())
            }
            Some(existing) if existing == identity => Ok(()),
            Some(_) => bail!(
                "mux RPC generation {} attempted to replace its established render connection identity",
                generation
            ),
        }
    }

    fn render_connection_identity(
        &self,
        generation: NonZeroU64,
    ) -> Option<RenderConnectionIdentity> {
        let lifecycle = self.lifecycle.lock();
        if matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) && self.live_generation.load(AtomicOrdering::Acquire) == generation.get()
        {
            lifecycle.render_connection_identity
        } else {
            None
        }
    }

    fn mark_incarnation_terminal(&self, error: RpcTransportError) -> RpcTransportError {
        debug_assert!(error.is_incarnation_terminal());
        let terminal_error = {
            let mut lifecycle = self.lifecycle.lock();
            if let Some(existing) = &lifecycle.terminal_error {
                return existing.clone();
            }
            let last_live = match lifecycle.phase {
                RpcTransportPhase::Live(generation) => generation,
                RpcTransportPhase::Reconnecting { retired, .. } => retired,
                RpcTransportPhase::Closed { last_live } => last_live,
            };
            self.ready_generation.store(0, AtomicOrdering::Release);
            self.live_generation.store(0, AtomicOrdering::Release);
            lifecycle.readiness_authority.retire();
            lifecycle.render_connection_identity = None;
            lifecycle.protocol = None;
            lifecycle.phase = RpcTransportPhase::Closed { last_live };
            lifecycle.terminal_error = Some(error.clone());
            error
        };
        let _ = self.terminal_reader_wake_tx.try_send(());
        terminal_error
    }

    fn terminal_error(&self) -> Option<RpcTransportError> {
        self.lifecycle.lock().terminal_error.clone()
    }

    fn codec_authority(&self, generation: NonZeroU64) -> Option<RpcCodecAuthority> {
        let lifecycle = self.lifecycle.lock();
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) || self.live_generation.load(AtomicOrdering::Acquire) != generation.get()
        {
            return None;
        }
        lifecycle
            .protocol_for(generation)
            .ok()
            .and_then(|protocol| protocol.codec)
    }

    fn reader_abort_for(
        &self,
        generation: NonZeroU64,
    ) -> anyhow::Result<Arc<RpcGenerationReaderAbortAuthority>> {
        let lifecycle = self.lifecycle.lock();
        if let Some(error) = &lifecycle.terminal_error {
            return Err(anyhow::Error::new(error.clone()));
        }
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) || self.live_generation.load(AtomicOrdering::Acquire) != generation.get()
            || lifecycle.reader_abort.generation != generation
        {
            bail!(
                "mux RPC reader abort authority for generation {} is not live",
                generation
            );
        }
        Ok(Arc::clone(&lifecycle.reader_abort))
    }

    fn reader_abort_for_reader(
        &self,
        generation: NonZeroU64,
    ) -> anyhow::Result<Arc<RpcGenerationReaderAbortAuthority>> {
        let lifecycle = self.lifecycle.lock();
        if let Some(error) = &lifecycle.terminal_error {
            return Err(anyhow::Error::new(error.clone()));
        }
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) || lifecycle.reader_abort.generation != generation
        {
            bail!(
                "mux RPC socket reader has no authority for generation {}",
                generation
            );
        }
        Ok(Arc::clone(&lifecycle.reader_abort))
    }

    fn validate_live_control_ack(
        &self,
        generation: NonZeroU64,
        reader_abort: &Arc<RpcGenerationReaderAbortAuthority>,
        operation: &'static str,
    ) -> anyhow::Result<()> {
        let lifecycle = self.lifecycle.lock();
        if let Some(error) = &lifecycle.terminal_error {
            return Err(anyhow::Error::new(error.clone()));
        }
        if let Some(error) = reader_abort.aborted_error() {
            return Err(error);
        }
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Live(observed) if observed == generation
        ) || self.live_generation.load(AtomicOrdering::Acquire) != generation.get()
            || reader_abort.generation != generation
            || !Arc::ptr_eq(&lifecycle.reader_abort, reader_abort)
        {
            bail!(
                "mux RPC {operation} acknowledgement lost exact live generation {generation} \
                 before caller observation"
            );
        }
        Ok(())
    }

    /// Commit an exact-generation cancellation and revoke admission before the
    /// reader is woken. The lifecycle intentionally remains `Live` until the
    /// owning reader executes the checked Live-to-Reconnecting transition.
    fn request_generation_abort(
        &self,
        authority: &Arc<RpcGenerationReaderAbortAuthority>,
        reason: &'static str,
    ) -> bool {
        let outcome = {
            let mut lifecycle = self.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == authority.generation
            ) || !Arc::ptr_eq(&lifecycle.reader_abort, authority)
            {
                "stale"
            } else if !authority.commit_abort(reason) {
                "already_committed"
            } else {
                self.ready_generation.store(0, AtomicOrdering::Release);
                self.live_generation.store(0, AtomicOrdering::Release);
                lifecycle.readiness_authority.retire();
                lifecycle.render_connection_identity = None;
                lifecycle.protocol = None;
                "committed"
            }
        };
        if outcome == "committed" {
            authority.wake_reader();
        }
        metrics::counter!(
            "mux.client.rpc.generation_abort.total",
            "outcome" => outcome,
        )
        .increment(1);
        outcome == "committed"
    }

    fn install_codec_authority(
        &self,
        generation: NonZeroU64,
        codec: RpcCodecAuthority,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for_mut(generation)?
            .install_codec(codec)
    }

    fn validate_dequeued_outbound(
        &self,
        generation: NonZeroU64,
        pdu: &Pdu,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for(generation)?
            .validate_outbound_pdu(pdu, RpcOutboundAdmissionPoint::Dequeue)
    }

    fn wire_dialect(
        &self,
        generation: NonZeroU64,
    ) -> Result<MuxWireDialect, OrdinaryMuxProtocolError> {
        Ok(self
            .lifecycle
            .lock()
            .protocol_for(generation)?
            .wire_dialect())
    }

    fn validate_inbound_header(
        &self,
        generation: NonZeroU64,
        header: &PduFrameHeader,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        let spec = Pdu::wire_spec_for_ident(header.ident()).ok_or(
            OrdinaryMuxProtocolError::UnknownPdu {
                direction: RpcProtocolDirection::Inbound,
                ident: header.ident(),
            },
        )?;
        let role = if header.serial() == 0 {
            PduWireRole::Unilateral
        } else {
            PduWireRole::CorrelatedReply
        };
        self.lifecycle
            .lock()
            .protocol_for(generation)?
            .validate_inbound(spec, role)
    }

    fn validate_inbound_identity(
        &self,
        generation: NonZeroU64,
        serial: u64,
        ident: u64,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        let spec = Pdu::wire_spec_for_ident(ident).ok_or(OrdinaryMuxProtocolError::UnknownPdu {
            direction: RpcProtocolDirection::Inbound,
            ident,
        })?;
        let role = if serial == 0 {
            PduWireRole::Unilateral
        } else {
            PduWireRole::CorrelatedReply
        };
        self.lifecycle
            .lock()
            .protocol_for(generation)?
            .validate_inbound(spec, role)
    }

    fn rollback_unadmitted_outbound(
        &self,
        generation: NonZeroU64,
        pdu: &Pdu,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for_mut(generation)?
            .rollback_unadmitted_outbound(pdu)
    }

    fn rollback_unadmitted_outbound_ident(
        &self,
        generation: NonZeroU64,
        ident: u64,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for_mut(generation)?
            .rollback_unadmitted_outbound_ident(ident);
        Ok(())
    }

    fn complete_protocol_response(
        &self,
        generation: NonZeroU64,
        request: &'static str,
        pdu: &Pdu,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for_mut(generation)?
            .complete_correlated_response(request, pdu)
    }

    fn establish_protocol_capabilities(
        &self,
        generation: NonZeroU64,
        capabilities: TopologyCapabilities,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        self.lifecycle
            .lock()
            .protocol_for_mut(generation)?
            .establish_capabilities(capabilities)
    }

    async fn complete_before_terminal<T>(
        &self,
        operation: impl Future<Output = T>,
    ) -> anyhow::Result<T> {
        if let Some(error) = self.terminal_error() {
            return Err(anyhow::Error::new(error));
        }
        let terminal = self.terminal_reader_wake_rx.recv();
        pin_mut!(operation);
        pin_mut!(terminal);
        match select(operation, terminal).await {
            Either::Left((result, _)) => Ok(result),
            Either::Right((wake, _)) => {
                wake.context("mux RPC terminal reader wake channel closed")?;
                let error = self
                    .terminal_error()
                    .ok_or_else(|| anyhow!("mux RPC reader woke without a terminal cause"))?;
                Err(anyhow::Error::new(error))
            }
        }
    }

    /// Snapshot the two reader-stop authorities under the lifecycle lock.
    ///
    /// Terminal incarnation failure permanently outranks a generation-local
    /// cancellation.  Both authorities can become observable in the same
    /// scheduler turn, so sampling them through separate lock acquisitions can
    /// return the lower-priority generation cause after terminal failure has
    /// already committed.
    fn reader_stop_error(
        &self,
        reader_abort: &RpcGenerationReaderAbortAuthority,
    ) -> Option<anyhow::Error> {
        let lifecycle = self.lifecycle.lock();
        if let Some(error) = &lifecycle.terminal_error {
            return Some(anyhow::Error::new(error.clone()));
        }
        reader_abort.aborted_error()
    }

    async fn complete_before_reader_stop<T>(
        &self,
        reader_abort: &RpcGenerationReaderAbortAuthority,
        operation: impl Future<Output = T>,
    ) -> anyhow::Result<T> {
        if let Some(error) = self.reader_stop_error(reader_abort) {
            return Err(error);
        }

        let operation = self.complete_before_terminal(operation);
        let generation_abort = reader_abort.cancelled();
        pin_mut!(operation);
        pin_mut!(generation_abort);
        let result = match select(operation, generation_abort).await {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => {
                Err(self.reader_stop_error(reader_abort).unwrap_or_else(|| {
                    anyhow!(
                        "mux RPC generation {} reader woke without a cancellation cause",
                        reader_abort.generation
                    )
                }))
            }
        };

        if let Some(error) = self.reader_stop_error(reader_abort) {
            return Err(error);
        }
        result
    }

    fn begin_consumer_commit(
        self: &Arc<Self>,
        generation: NonZeroU64,
        consumer: RpcConsumerKind,
    ) -> Result<RpcGenerationCommitLease, RpcConsumerCommitError> {
        let admission = {
            let mut lifecycle = self.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self.live_generation.load(AtomicOrdering::Acquire) != generation.get()
            {
                Err(RpcConsumerCommitError::Retired {
                    consumer,
                    bound_generation: generation,
                    active_generation: self.active_generation(),
                })
            } else {
                match lifecycle.active_consumer_commits.checked_add(1) {
                    Some(active) => {
                        lifecycle.active_consumer_commits = active;
                        Ok(RpcGenerationCommitLease {
                            rpc_transport: Arc::clone(self),
                            consumer,
                        })
                    }
                    None => Err(RpcConsumerCommitError::AccountingOverflow {
                        consumer,
                        generation,
                    }),
                }
            }
        };
        let outcome = match &admission {
            Ok(_) => "admitted",
            Err(RpcConsumerCommitError::Retired { .. }) => "retired",
            Err(RpcConsumerCommitError::AccountingOverflow { .. }) => "accounting_overflow",
            Err(RpcConsumerCommitError::Unavailable { .. }) => "unavailable",
        };
        metrics::counter!(
            "mux.client.rpc.consumer_commit.total",
            "consumer" => consumer.metric_label(),
            "outcome" => outcome
        )
        .increment(1);
        admission
    }

    fn validate(
        &self,
        binding: RpcBinding,
        stage: RpcRetirementStage,
        certainty: RpcDeliveryCertainty,
        reason: impl Into<String>,
    ) -> Result<(), RpcTransportError> {
        if self.live_generation.load(AtomicOrdering::Acquire) == binding.generation.get() {
            return Ok(());
        }

        Err(self.make_retirement_error(binding, stage, certainty, reason))
    }

    fn make_retirement_error(
        &self,
        binding: RpcBinding,
        stage: RpcRetirementStage,
        certainty: RpcDeliveryCertainty,
        reason: impl Into<String>,
    ) -> RpcTransportError {
        RpcTransportError::Retired {
            attempt_id: binding.attempt_id,
            request: binding.request,
            bound_generation: binding.generation,
            active_generation: self.active_generation(),
            stage,
            certainty,
            reason: reason.into(),
        }
    }

    fn retirement_error(
        &self,
        binding: RpcBinding,
        stage: RpcRetirementStage,
        certainty: RpcDeliveryCertainty,
        reason: impl Into<String>,
    ) -> RpcTransportError {
        let error = self.make_retirement_error(binding, stage, certainty, reason);
        record_rpc_transport_error(&error);
        error
    }

    fn unavailable_error(
        attempt_id: NonZeroU64,
        request: &'static str,
        stage: RpcRetirementStage,
    ) -> RpcTransportError {
        let error = RpcTransportError::Unavailable {
            attempt_id,
            request,
            stage,
        };
        record_rpc_transport_error(&error);
        error
    }

    #[cfg(test)]
    fn mark_current_generation_ready_for_test(&self) {
        self.mark_current_generation_ready_with_codec_for_test(CODEC_VERSION);
    }

    #[cfg(test)]
    fn mark_current_generation_ready_with_codec_for_test(&self, agreed_codec_version: usize) {
        let generation = self
            .active_generation()
            .expect("test RPC transport generation should be live");
        let readiness_authority = {
            let mut lifecycle = self.lifecycle.lock();
            lifecycle.protocol = Some(RpcProtocolAuthority::established_for_test(
                generation,
                agreed_codec_version,
            ));
            Arc::clone(&lifecycle.readiness_authority)
        };
        let participating = readiness_authority
            .register_participant()
            .expect("test readiness participant should register");
        readiness_authority
            .mark_ready()
            .expect("test readiness authority should accept its live generation");
        if participating {
            let _ = readiness_authority.release_participant(false);
        }
        self.ready_generation
            .store(generation.get(), AtomicOrdering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopologySnapshotDecisionAck {
    CommittedLive,
    RejectedTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyTopologyFenceAuthority {
    generation: NonZeroU64,
    serial: NonZeroU64,
}

#[derive(Debug)]
enum PendingRpcReply {
    Pdu(Box<Pdu>),
    Legacy46ListPanesResponse {
        response: Legacy46ListPanesResponse,
        authority: LegacyTopologyFenceAuthority,
    },
    Legacy46Rejection(Legacy46Rejection),
}

/// Topology payload delivered to the domain under the exact snapshot
/// authority available from its peer.
pub(crate) enum RpcTopologySnapshot {
    Current(ListPanesResponse),
    Legacy46(Legacy46ListPanesResponse),
}

impl PendingRpcReply {
    fn pdu(pdu: Pdu) -> Self {
        Self::Pdu(Box::new(pdu))
    }

    fn response_name(&self) -> &'static str {
        match self {
            Self::Pdu(pdu) => pdu.pdu_name(),
            Self::Legacy46ListPanesResponse { .. } => "Legacy46ListPanesResponse",
            Self::Legacy46Rejection(_) => "Legacy46Rejection",
        }
    }
}

enum ReaderMessage {
    SendPdu {
        binding: RpcBinding,
        lease: ClientOutboundLease,
        promise: Sender<anyhow::Result<PendingRpcReply>>,
    },
    PublishReady {
        generation: NonZeroU64,
        reader_sender: Sender<ReaderMessage>,
        promise: Sender<anyhow::Result<()>>,
        reservation: RpcReadinessPublicationLease,
    },
    FinishReadyReplay {
        generation: NonZeroU64,
        reader_sender: Sender<ReaderMessage>,
        replayed_pdus: usize,
        replayed_bytes: usize,
        result: anyhow::Result<()>,
    },
    CommitTopologySnapshot {
        generation: NonZeroU64,
        authority: TopologyFenceAuthority,
        promise: Sender<anyhow::Result<TopologySnapshotDecisionAck>>,
    },
    RejectTopologySnapshot {
        generation: NonZeroU64,
        authority: TopologyFenceAuthority,
        promise: Sender<anyhow::Result<TopologySnapshotDecisionAck>>,
    },
    CommitLegacyTopologySnapshot {
        generation: NonZeroU64,
        authority: LegacyTopologyFenceAuthority,
        promise: Sender<anyhow::Result<()>>,
    },
}

impl ReaderMessage {
    fn retire(self, rpc_transport: &RpcTransportState, stage: RpcRetirementStage, reason: &str) {
        match self {
            Self::SendPdu {
                binding, promise, ..
            } => {
                let error = rpc_transport.retirement_error(
                    binding,
                    stage,
                    RpcDeliveryCertainty::DefinitelyNotSent,
                    reason,
                );
                let _ = promise.try_send(Err(anyhow::Error::new(error)));
            }
            Self::PublishReady { promise, .. } => {
                let _ = promise.try_send(Err(anyhow!(
                    "mux RPC readiness publication retired before reader admission: {reason}"
                )));
            }
            Self::FinishReadyReplay { .. } => {}
            Self::CommitTopologySnapshot { promise, .. }
            | Self::RejectTopologySnapshot { promise, .. } => {
                let _ = promise.try_send(Err(anyhow!(
                    "mux topology snapshot decision retired before reader admission: {reason}"
                )));
            }
            Self::CommitLegacyTopologySnapshot { promise, .. } => {
                let _ = promise.try_send(Err(anyhow!(
                    "legacy mux topology snapshot commit retired before reader admission: {reason}"
                )));
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    sender: Sender<ReaderMessage>,
    local_domain_id: Option<DomainId>,
    incarnation: Arc<ClientIncarnation>,
    connection_generation: Arc<AtomicU64>,
    rpc_transport: Arc<RpcTransportState>,
    domain_reconnect_authorized: Arc<AtomicBool>,
    pub client_id: ClientId,
    client_domain_config: ClientDomainConfig,
    pub is_reconnectable: bool,
    pub is_local: bool,
}

/// Reusable exact-generation authority for a related group of mux RPCs.
///
/// Capturing a scope while no transport is live produces a permanently
/// unavailable scope; it never upgrades itself onto a successor connection.
/// Each call still receives a fresh attempt identity and remains lazy until
/// first poll.
#[derive(Clone)]
pub(crate) struct RpcGenerationScope {
    sender: Sender<ReaderMessage>,
    rpc_transport: Arc<RpcTransportState>,
    generation: Option<NonZeroU64>,
    reader_abort: Option<Arc<RpcGenerationReaderAbortAuthority>>,
    allow_unready: bool,
}

/// Cancellation-safe retirement for a bootstrap operation that may have
/// received a state-subsuming response but has not yet published readiness.
pub(crate) struct RpcGenerationAbortGuard {
    rpc_transport: Arc<RpcTransportState>,
    reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    readiness_authority: Option<Arc<RpcReadinessAuthority>>,
    generation: NonZeroU64,
    reason: &'static str,
    armed: bool,
    fatal: bool,
}

/// Cancellation guard for a delivered coherent topology snapshot.
///
/// A snapshot response is not authority to discard any event until its exact
/// consumer has applied the snapshot and the reader has acknowledged the
/// matching commit. Dropping the consumer anywhere inside that interval
/// therefore revokes admission and wakes the owning reader directly. Reader
/// teardown discards the uncommitted topology state without pruning it.
struct TopologySnapshotDecisionGuard {
    rpc_transport: Arc<RpcTransportState>,
    reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    armed: bool,
}

struct TopologySnapshotRequestGuard {
    rpc_transport: Arc<RpcTransportState>,
    reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    armed: bool,
}

impl TopologySnapshotRequestGuard {
    fn new(
        rpc_transport: Arc<RpcTransportState>,
        reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    ) -> Self {
        Self {
            rpc_transport,
            reader_abort,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TopologySnapshotRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.rpc_transport.request_generation_abort(
                &self.reader_abort,
                "coherent topology snapshot cancelled before exact consumer decision",
            );
        }
    }
}

impl TopologySnapshotDecisionGuard {
    fn new(
        rpc_transport: Arc<RpcTransportState>,
        reader_abort: Arc<RpcGenerationReaderAbortAuthority>,
    ) -> Self {
        Self {
            rpc_transport,
            reader_abort,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TopologySnapshotDecisionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.rpc_transport.request_generation_abort(
            &self.reader_abort,
            "coherent topology snapshot cancelled after response delivery",
        );
    }
}

impl RpcGenerationAbortGuard {
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    fn authorizes_pending_readiness(
        &self,
        rpc_transport: &Arc<RpcTransportState>,
        generation: NonZeroU64,
        readiness_authority: &Arc<RpcReadinessAuthority>,
    ) -> bool {
        self.armed
            && !self.fatal
            && self.generation == generation
            && Arc::ptr_eq(&self.rpc_transport, rpc_transport)
            && self
                .readiness_authority
                .as_ref()
                .is_some_and(|guard_authority| Arc::ptr_eq(guard_authority, readiness_authority))
    }
}

impl Drop for RpcGenerationAbortGuard {
    fn drop(&mut self) {
        let should_abort = if self.fatal {
            self.armed
                && self
                    .readiness_authority
                    .as_ref()
                    .is_some_and(|authority| authority.commit_fatal_abort())
        } else {
            self.readiness_authority
                .as_ref()
                .is_some_and(|authority| authority.release_participant(self.armed))
        };
        if !should_abort {
            return;
        }
        self.rpc_transport
            .request_generation_abort(&self.reader_abort, self.reason);
    }
}

impl RpcGenerationScope {
    fn capture(sender: Sender<ReaderMessage>, rpc_transport: Arc<RpcTransportState>) -> Self {
        let (generation, reader_abort) = {
            let lifecycle = rpc_transport.lifecycle.lock();
            match lifecycle.phase {
                RpcTransportPhase::Live(generation)
                    if rpc_transport.live_generation.load(AtomicOrdering::Acquire)
                        == generation.get()
                        && rpc_transport.ready_generation.load(AtomicOrdering::Acquire)
                            == generation.get() =>
                {
                    (Some(generation), Some(Arc::clone(&lifecycle.reader_abort)))
                }
                _ => (None, None),
            }
        };
        Self {
            sender,
            rpc_transport,
            generation,
            reader_abort,
            allow_unready: false,
        }
    }

    fn exact(
        sender: Sender<ReaderMessage>,
        rpc_transport: Arc<RpcTransportState>,
        generation: NonZeroU64,
        allow_unready: bool,
    ) -> Self {
        let reader_abort = rpc_transport.reader_abort_for(generation).ok();
        Self {
            sender,
            rpc_transport,
            generation: reader_abort.as_ref().map(|_| generation),
            reader_abort,
            allow_unready,
        }
    }

    fn bootstrap(sender: Sender<ReaderMessage>, rpc_transport: Arc<RpcTransportState>) -> Self {
        let (generation, reader_abort) = {
            let lifecycle = rpc_transport.lifecycle.lock();
            match lifecycle.phase {
                RpcTransportPhase::Live(generation)
                    if rpc_transport.live_generation.load(AtomicOrdering::Acquire)
                        == generation.get() =>
                {
                    (Some(generation), Some(Arc::clone(&lifecycle.reader_abort)))
                }
                _ => (None, None),
            }
        };
        Self {
            sender,
            rpc_transport,
            generation,
            reader_abort,
            allow_unready: true,
        }
    }

    pub(crate) fn same_generation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.rpc_transport, &other.rpc_transport)
            && self.generation.is_some()
            && self.generation == other.generation
            && self
                .reader_abort
                .as_ref()
                .zip(other.reader_abort.as_ref())
                .is_some_and(|(left, right)| Arc::ptr_eq(left, right))
    }

    pub(crate) fn is_available(&self) -> bool {
        self.generation.is_some()
    }

    pub(crate) const fn connection_generation(&self) -> Option<NonZeroU64> {
        self.generation
    }

    fn codec_authority(&self) -> Option<RpcCodecAuthority> {
        self.generation
            .and_then(|generation| self.rpc_transport.codec_authority(generation))
    }

    /// Return the negotiated codec owned by this exact RPC generation.
    ///
    /// Unlike [`Client::agreed_codec_version`], this never re-captures ambient
    /// transport state, so callers can make a dialect choice and bind the
    /// corresponding RPC to one indivisible generation authority.
    pub(crate) fn agreed_codec_version(&self) -> Option<usize> {
        self.codec_authority().map(|authority| authority.agreed)
    }

    fn retain_codec_authority(
        &self,
        codec: RpcCodecAuthority,
    ) -> Result<(), OrdinaryMuxProtocolError> {
        let generation =
            self.generation
                .ok_or(OrdinaryMuxProtocolError::ProtocolAuthorityUnavailable {
                    generation: codec.generation,
                })?;
        self.rpc_transport
            .install_codec_authority(generation, codec)
    }

    pub(crate) fn render_connection_identity(&self) -> Option<RenderConnectionIdentity> {
        self.generation
            .and_then(|generation| self.rpc_transport.render_connection_identity(generation))
    }

    pub(crate) fn commit_sync<T>(
        &self,
        consumer: RpcConsumerKind,
        commit: impl FnOnce() -> T,
    ) -> Result<T, RpcConsumerCommitError> {
        let Some(generation) = self.generation else {
            metrics::counter!(
                "mux.client.rpc.consumer_commit.total",
                "consumer" => consumer.metric_label(),
                "outcome" => "unavailable"
            )
            .increment(1);
            return Err(RpcConsumerCommitError::Unavailable { consumer });
        };
        let _lease = self
            .rpc_transport
            .begin_consumer_commit(generation, consumer)?;
        Ok(commit())
    }

    pub(crate) fn abort_guard(
        &self,
        reason: &'static str,
    ) -> anyhow::Result<RpcGenerationAbortGuard> {
        let generation = self
            .generation
            .ok_or_else(|| anyhow!("cannot guard an unavailable mux RPC scope"))?;
        let scoped_reader_abort = self
            .reader_abort
            .as_ref()
            .ok_or_else(|| anyhow!("mux RPC scope has no exact reader abort authority"))?;
        let (readiness_authority, reader_abort) = {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                != generation.get()
                || !Arc::ptr_eq(&lifecycle.reader_abort, scoped_reader_abort)
            {
                bail!(
                    "cannot register readiness participant for retired mux RPC generation {}",
                    generation
                );
            }
            if lifecycle.readiness_authority.generation != generation {
                bail!(
                    "mux RPC readiness authority generation {} does not match scope {}",
                    lifecycle.readiness_authority.generation,
                    generation
                );
            }
            (
                Arc::clone(&lifecycle.readiness_authority),
                Arc::clone(&lifecycle.reader_abort),
            )
        };
        let participating = readiness_authority.register_participant()?;
        Ok(RpcGenerationAbortGuard {
            rpc_transport: Arc::clone(&self.rpc_transport),
            reader_abort,
            readiness_authority: participating.then_some(readiness_authority),
            generation,
            reason,
            armed: true,
            fatal: false,
        })
    }

    fn fatal_abort_guard(&self, reason: &'static str) -> anyhow::Result<RpcGenerationAbortGuard> {
        let generation = self
            .generation
            .ok_or_else(|| anyhow!("cannot guard an unavailable mux RPC scope"))?;
        let scoped_reader_abort = self
            .reader_abort
            .as_ref()
            .ok_or_else(|| anyhow!("mux RPC scope has no exact reader abort authority"))?;
        let (readiness_authority, reader_abort) = {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                != generation.get()
                || lifecycle.readiness_authority.generation != generation
                || !Arc::ptr_eq(&lifecycle.reader_abort, scoped_reader_abort)
            {
                bail!(
                    "cannot register fatal readiness guard for retired mux RPC generation {}",
                    generation
                );
            }
            (
                Arc::clone(&lifecycle.readiness_authority),
                Arc::clone(&lifecycle.reader_abort),
            )
        };
        Ok(RpcGenerationAbortGuard {
            rpc_transport: Arc::clone(&self.rpc_transport),
            reader_abort,
            readiness_authority: Some(readiness_authority),
            generation,
            reason,
            armed: true,
            fatal: true,
        })
    }

    fn send_pdu(
        &self,
        pdu: Pdu,
    ) -> impl std::future::Future<Output = anyhow::Result<Pdu>> + Send + 'static {
        self.send_pdu_expect(pdu, None)
    }

    fn send_pdu_expect_reply(
        &self,
        pdu: Pdu,
        expected_response_ident: Option<NonZeroU64>,
    ) -> impl std::future::Future<Output = anyhow::Result<PendingRpcReply>> + Send + 'static {
        let request = pdu.pdu_name();
        let rpc_transport = Arc::clone(&self.rpc_transport);
        let sender = self.sender.clone();
        let scoped_generation = self.generation;
        let allow_unready = self.allow_unready;
        // Cheap generation/dialect validation precedes potentially large
        // schema counting. The second validation cut below closes a retirement
        // race while the immutable PDU was being measured.
        let attempt = rpc_transport.allocate_attempt(request);
        let binding: anyhow::Result<(RpcBinding, MuxWireDialect)> =
            attempt.map_err(anyhow::Error::new).and_then(|attempt_id| {
                let Some(generation) = scoped_generation else {
                    return Err(anyhow::Error::new(RpcTransportState::unavailable_error(
                        attempt_id,
                        request,
                        RpcRetirementStage::Admission,
                    )));
                };
                let lifecycle = rpc_transport.lifecycle.lock();
                if !matches!(
                    lifecycle.phase,
                    RpcTransportPhase::Live(observed) if observed == generation
                ) || rpc_transport.live_generation.load(AtomicOrdering::Acquire)
                    != generation.get()
                {
                    return Err(anyhow::Error::new(rpc_transport.retirement_error(
                        RpcBinding {
                            generation,
                            attempt_id,
                            request,
                            expected_response_ident,
                        },
                        RpcRetirementStage::Admission,
                        RpcDeliveryCertainty::DefinitelyNotSent,
                        "exact-generation RPC scope is no longer live",
                    )));
                }
                if !allow_unready
                    && rpc_transport.ready_generation.load(AtomicOrdering::Acquire)
                        != generation.get()
                {
                    return Err(anyhow::Error::new(RpcTransportState::unavailable_error(
                        attempt_id,
                        request,
                        RpcRetirementStage::Admission,
                    )));
                }
                let protocol = lifecycle
                    .protocol_for(generation)
                    .map_err(anyhow::Error::new)?;
                protocol
                    .validate_outbound_pdu(&pdu, RpcOutboundAdmissionPoint::Preflight)
                    .map_err(|error| {
                        record_ordinary_mux_protocol_rejection(&error, "outbound", "preflight");
                        anyhow::Error::new(error)
                    })?;
                Ok((
                    RpcBinding {
                        generation,
                        attempt_id,
                        request,
                        expected_response_ident,
                    },
                    protocol.wire_dialect(),
                ))
            });
        // Planning and the worst-case logical-byte reservation both happen
        // synchronously, before this future can allocate a wire serial, touch
        // the pending map, enter the queue, serialize, compress, or write.
        let admission: anyhow::Result<(RpcBinding, ClientOutboundLease)> =
            binding.and_then(|(binding, dialect)| {
                let prepared = pdu
                    .prepare_outbound_for_dialect(
                        dialect,
                        PduProducer::Client,
                        PduWireRole::Request,
                        None,
                        CompressionMode::Auto,
                    )
                    .map_err(anyhow::Error::new)?;
                {
                    let lifecycle = rpc_transport.lifecycle.lock();
                    if !matches!(
                        lifecycle.phase,
                        RpcTransportPhase::Live(observed) if observed == binding.generation
                    ) || rpc_transport.live_generation.load(AtomicOrdering::Acquire)
                        != binding.generation.get()
                        || (!allow_unready
                            && rpc_transport.ready_generation.load(AtomicOrdering::Acquire)
                                != binding.generation.get())
                    {
                        return Err(anyhow::Error::new(rpc_transport.retirement_error(
                            binding,
                            RpcRetirementStage::Admission,
                            RpcDeliveryCertainty::DefinitelyNotSent,
                            "exact-generation RPC retired while its outbound PDU was being planned",
                        )));
                    }
                    let protocol = lifecycle
                        .protocol_for(binding.generation)
                        .map_err(anyhow::Error::new)?;
                    protocol
                        .validate_outbound_pdu(
                            prepared.pdu(),
                            RpcOutboundAdmissionPoint::Preflight,
                        )
                        .map_err(|error| {
                            record_ordinary_mux_protocol_rejection(&error, "outbound", "preflight");
                            anyhow::Error::new(error)
                        })?;
                    if protocol.wire_dialect() != prepared.dialect() {
                        bail!(
                            "mux RPC generation {} changed wire dialect from {} to {} while the exact request was being planned",
                            binding.generation,
                            prepared.dialect().codec_version(),
                            protocol.wire_dialect().codec_version(),
                        );
                    }
                }
                let lease = rpc_transport
                    .outbound_budget
                    .try_reserve(Arc::downgrade(&rpc_transport), binding.generation, prepared)
                    .map_err(anyhow::Error::new)?;
                Ok((binding, lease))
            });
        async move {
            let (binding, lease) = match admission {
                Ok(admission) => admission,
                Err(error) => return Err(error),
            };
            // Once the request enters the physical queue, dropping the caller
            // future cancels and settles the reservation only while the reader
            // has not claimed it. After that claim, the reader retains the
            // budget through encoding and completed write/flush teardown.
            let cancellation_guard = lease.cancellation_guard();
            let (promise, rx) = bounded(1);
            // Hold the short admission gate through the nonblocking enqueue.
            // Retirement takes the same gate before publishing Reconnecting,
            // so bind-then-enqueue cannot straddle transport generations.
            let rejected_message = {
                let mut lifecycle = rpc_transport.lifecycle.lock();
                if !matches!(
                    lifecycle.phase,
                    RpcTransportPhase::Live(generation) if generation == binding.generation
                ) || rpc_transport.live_generation.load(AtomicOrdering::Acquire)
                    != binding.generation.get()
                    || (!allow_unready
                        && rpc_transport.ready_generation.load(AtomicOrdering::Acquire)
                            != binding.generation.get())
                {
                    return Err(anyhow::Error::new(rpc_transport.retirement_error(
                        binding,
                        RpcRetirementStage::Enqueue,
                        RpcDeliveryCertainty::DefinitelyNotSent,
                        "bound RPC was first polled after its transport retired",
                    )));
                }
                let protocol = lifecycle
                    .protocol_for_mut(binding.generation)
                    .map_err(anyhow::Error::new)?;
                lease
                    .with_prepared(|prepared| protocol.admit_outbound(prepared.pdu()))?
                    .map_err(|error| {
                        record_ordinary_mux_protocol_rejection(&error, "outbound", "enqueue");
                        anyhow::Error::new(error)
                    })?;
                lease.arm_protocol_rollback();
                match sender.try_send(ReaderMessage::SendPdu {
                    binding,
                    lease,
                    promise,
                }) {
                    Ok(()) => None,
                    Err(TrySendError::Closed(message) | TrySendError::Full(message)) => {
                        Some(message)
                    }
                }
            };
            if let Some(message) = rejected_message {
                // Dropping the rejected queue node releases its byte/slot
                // lease and emits metrics. Keep both operations outside the
                // lifecycle critical section.
                drop(message);
                return Err(anyhow::Error::new(rpc_transport.retirement_error(
                    binding,
                    RpcRetirementStage::Enqueue,
                    RpcDeliveryCertainty::DefinitelyNotSent,
                    "RPC queue was unavailable during exact-generation admission",
                )));
            }
            let result = match rx.recv().await {
                // The physical reader validates the exact generation and the
                // typed serial correlation before it removes the pending RPC
                // and enqueues this value. That successful enqueue is the
                // response-delivery linearization point. A later socket EOF
                // may retire the transport before this task is scheduled, but
                // it cannot revoke an already decoded and correlated reply.
                Ok(Ok(pdu)) => Ok(pdu),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(anyhow::Error::new(rpc_transport.retirement_error(
                    binding,
                    RpcRetirementStage::CompletionChannel,
                    RpcDeliveryCertainty::OutcomeUnknown,
                    "RPC completion channel closed without a terminal result",
                ))),
            };
            drop(cancellation_guard);
            result
        }
    }

    fn send_pdu_expect(
        &self,
        pdu: Pdu,
        expected_response_ident: Option<NonZeroU64>,
    ) -> impl std::future::Future<Output = anyhow::Result<Pdu>> + Send + 'static {
        let request_name = pdu.pdu_name();
        let request = self.send_pdu_expect_reply(pdu, expected_response_ident);
        async move {
            match request.await? {
                PendingRpcReply::Pdu(pdu) => Ok(*pdu),
                PendingRpcReply::Legacy46Rejection(rejection) => {
                    Err(anyhow::Error::new(Legacy46RpcRejection {
                        request: request_name,
                        effect_authority: rejection.effect_authority(),
                        retry_authority: rejection.retry_authority(),
                    }))
                }
                PendingRpcReply::Legacy46ListPanesResponse { .. } => {
                    bail!("legacy topology reply escaped its snapshot consumer for {request_name}")
                }
            }
        }
    }

    async fn decide_topology_snapshot(
        &self,
        authority: TopologyFenceAuthority,
        commit: bool,
    ) -> anyhow::Result<()> {
        let generation = self
            .generation
            .ok_or_else(|| anyhow!("cannot decide a topology snapshot on an unavailable scope"))?;
        let reader_abort = self
            .reader_abort
            .as_ref()
            .filter(|authority| authority.generation == generation)
            .cloned()
            .ok_or_else(|| anyhow!("topology snapshot decision lacks exact reader authority"))?;
        let (promise, receiver) = bounded(1);
        {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                != generation.get()
                || !Arc::ptr_eq(&lifecycle.reader_abort, &reader_abort)
            {
                bail!(
                    "topology snapshot decision lost exact generation {} before enqueue",
                    generation
                );
            }
            let message = if commit {
                ReaderMessage::CommitTopologySnapshot {
                    generation,
                    authority,
                    promise,
                }
            } else {
                ReaderMessage::RejectTopologySnapshot {
                    generation,
                    authority,
                    promise,
                }
            };
            self.sender.try_send(message).map_err(|_| {
                anyhow!(
                    "mux RPC reader queue closed before topology snapshot {}",
                    if commit { "commit" } else { "rejection" }
                )
            })?;
        }
        let acknowledgement = receiver.recv().await.map_err(|_| {
            anyhow!(
                "mux RPC reader closed without acknowledging topology snapshot {}",
                if commit { "commit" } else { "rejection" }
            )
        })??;
        match (commit, acknowledgement) {
            (true, TopologySnapshotDecisionAck::CommittedLive) => self
                .rpc_transport
                .validate_live_control_ack(generation, &reader_abort, "topology snapshot commit"),
            (false, TopologySnapshotDecisionAck::RejectedTerminal) => Ok(()),
            (expected_commit, observed) => bail!(
                "mux RPC topology snapshot decision acknowledgement mismatch: expected {}, \
                 observed {observed:?}",
                if expected_commit {
                    "committed-live"
                } else {
                    "rejected-terminal"
                }
            ),
        }
    }

    async fn commit_legacy_topology_snapshot(
        &self,
        authority: LegacyTopologyFenceAuthority,
    ) -> anyhow::Result<()> {
        let generation = self.generation.ok_or_else(|| {
            anyhow!("cannot commit a legacy topology snapshot on an unavailable scope")
        })?;
        if authority.generation != generation {
            bail!(
                "legacy topology authority generation {} does not match scope {}",
                authority.generation,
                generation
            );
        }
        let reader_abort = self
            .reader_abort
            .as_ref()
            .filter(|reader| reader.generation == generation)
            .cloned()
            .ok_or_else(|| anyhow!("legacy topology commit lacks exact reader authority"))?;
        let (promise, receiver) = bounded(1);
        {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                != generation.get()
                || !Arc::ptr_eq(&lifecycle.reader_abort, &reader_abort)
            {
                bail!(
                    "legacy topology commit lost exact generation {} before enqueue",
                    generation
                );
            }
            self.sender
                .try_send(ReaderMessage::CommitLegacyTopologySnapshot {
                    generation,
                    authority,
                    promise,
                })
                .map_err(|_| anyhow!("legacy topology commit reader queue is unavailable"))?;
        }
        receiver
            .recv()
            .await
            .context("legacy topology commit acknowledgement channel closed")??;
        Ok(())
    }

    /// Fetch, apply, and commit one exact-generation coherent topology snapshot.
    ///
    /// The per-transport gate remains held from request admission through the
    /// reader's commit acknowledgement. A failed or cancelled consumer revokes
    /// and wakes the owning connection generation without pruning any buffered
    /// event.
    pub(crate) async fn with_coherent_topology_snapshot<T>(
        &self,
        consumer: RpcConsumerKind,
        apply: impl FnOnce(RpcTopologySnapshot) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let generation = self
            .generation
            .ok_or_else(|| anyhow!("cannot snapshot topology on an unavailable mux RPC scope"))?;
        let dialect = self
            .rpc_transport
            .wire_dialect(generation)
            .map_err(anyhow::Error::new)?;
        if dialect.is_legacy46() {
            return self.with_legacy_topology_snapshot(consumer, apply).await;
        }
        let reader_abort = self
            .reader_abort
            .as_ref()
            .filter(|authority| authority.generation == generation)
            .cloned()
            .ok_or_else(|| anyhow!("coherent topology snapshot lacks reader abort authority"))?;
        let _topology_gate = self.rpc_transport.topology_sync.lock().await;
        let mut request_guard = TopologySnapshotRequestGuard::new(
            Arc::clone(&self.rpc_transport),
            Arc::clone(&reader_abort),
        );
        let response = self
            .list_panes_coherent(ListPanesCoherent {
                supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            })
            .await?;
        let authority = matches!(&response.outcome, ListPanesCoherentOutcome::Snapshot(_))
            .then(|| TopologyFenceAuthority::from_response(&response))
            .transpose()?;
        let snapshot = match response.outcome {
            ListPanesCoherentOutcome::Snapshot(snapshot) => snapshot,
            ListPanesCoherentOutcome::Contended {
                attempts,
                first_revision,
                last_revision,
            } => {
                request_guard.disarm();
                bail!(
                    "coherent topology snapshot remained contended after {attempts} attempts \
                     (revision {} -> {})",
                    first_revision.get(),
                    last_revision.get()
                )
            }
            ListPanesCoherentOutcome::RevisionExhausted => {
                request_guard.disarm();
                bail!("server topology revision authority is exhausted")
            }
            ListPanesCoherentOutcome::Unsupported { supported } => {
                request_guard.disarm();
                bail!(
                    "server cannot provide the required coherent topology fence \
                     (supported bits {:#x})",
                    supported.bits()
                )
            }
        };
        let authority =
            authority.expect("snapshot outcome must carry validated topology authority");
        let mut decision =
            TopologySnapshotDecisionGuard::new(Arc::clone(&self.rpc_transport), reader_abort);
        request_guard.disarm();
        let applied = self
            .commit_sync(consumer, || {
                apply(RpcTopologySnapshot::Current(snapshot.panes))
            })
            .map_err(anyhow::Error::new)?;
        match applied {
            Ok(value) => {
                self.decide_topology_snapshot(authority, true).await?;
                decision.disarm();
                Ok(value)
            }
            Err(error) => {
                match self.decide_topology_snapshot(authority, false).await {
                    Ok(()) => decision.disarm(),
                    Err(reject_error) => {
                        return Err(error).context(format!(
                            "topology snapshot application failed and rejection was not \
                             acknowledged: {reject_error:#}"
                        ));
                    }
                }
                Err(error).context("applying coherent topology snapshot")
            }
        }
    }

    async fn with_legacy_topology_snapshot<T>(
        &self,
        consumer: RpcConsumerKind,
        apply: impl FnOnce(RpcTopologySnapshot) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let generation = self.generation.ok_or_else(|| {
            anyhow!("cannot snapshot legacy topology on an unavailable mux RPC scope")
        })?;
        let reader_abort = self
            .reader_abort
            .as_ref()
            .filter(|authority| authority.generation == generation)
            .cloned()
            .ok_or_else(|| anyhow!("legacy topology snapshot lacks reader abort authority"))?;
        let _topology_gate = self.rpc_transport.topology_sync.lock().await;
        let mut request_guard = TopologySnapshotRequestGuard::new(
            Arc::clone(&self.rpc_transport),
            Arc::clone(&reader_abort),
        );
        let reply = self
            .send_pdu_expect_reply(
                Pdu::ListPanes(ListPanes {}),
                Some(
                    NonZeroU64::new(<ListPanesResponse as PduWireIdent>::IDENT)
                        .expect("ListPanesResponse has a nonzero wire identity"),
                ),
            )
            .await?;
        let (response, authority) = match reply {
            PendingRpcReply::Legacy46ListPanesResponse {
                response,
                authority,
            } => (response, authority),
            PendingRpcReply::Legacy46Rejection(rejection) => {
                return Err(anyhow::Error::new(Legacy46RpcRejection {
                    request: "ListPanes",
                    effect_authority: rejection.effect_authority(),
                    retry_authority: rejection.retry_authority(),
                }));
            }
            PendingRpcReply::Pdu(other) => {
                bail!(
                    "unexpected {} response to codec-46 ListPanes",
                    other.pdu_name()
                );
            }
        };
        let mut decision =
            TopologySnapshotDecisionGuard::new(Arc::clone(&self.rpc_transport), reader_abort);
        request_guard.disarm();
        let applied = self
            .commit_sync(consumer, || apply(RpcTopologySnapshot::Legacy46(response)))
            .map_err(anyhow::Error::new)?;
        match applied {
            Ok(value) => {
                self.commit_legacy_topology_snapshot(authority).await?;
                decision.disarm();
                Ok(value)
            }
            Err(error) => Err(error).context("applying codec-46 topology snapshot"),
        }
    }
}

struct ClientIncarnation;

#[derive(Clone)]
enum ClientDispatchTarget {
    Standalone,
    Attached {
        local_domain_id: DomainId,
        mux_owner: Weak<Mux>,
    },
}

/// Exact authority for dispatching work produced by one transport connection.
///
/// The client incarnation prevents an old reconnect thread from operating on a
/// replacement `ClientInner`. The monotonically increasing connection
/// generation revokes already-queued work as soon as its transport ends. The
/// weak mux owner prevents process-global mux replacement from retargeting that
/// work.
#[derive(Clone)]
struct ClientDispatchAuthority {
    target: ClientDispatchTarget,
    client_incarnation: Arc<ClientIncarnation>,
    connection_generation: Arc<AtomicU64>,
    rpc_transport: Arc<RpcTransportState>,
    generation: u64,
}

struct CurrentClientDispatch {
    authority: ClientDispatchAuthority,
    mux: Arc<Mux>,
    domain: DomainOperationGuard,
    inner: Arc<ClientInner>,
}

impl ClientDispatchAuthority {
    fn new(
        local_domain_id: Option<DomainId>,
        mux_owner: Weak<Mux>,
        client_incarnation: Arc<ClientIncarnation>,
        connection_generation: Arc<AtomicU64>,
        rpc_transport: Arc<RpcTransportState>,
    ) -> Self {
        let target = match local_domain_id {
            Some(local_domain_id) => ClientDispatchTarget::Attached {
                local_domain_id,
                mux_owner,
            },
            None => ClientDispatchTarget::Standalone,
        };
        Self {
            target,
            client_incarnation,
            connection_generation: Arc::clone(&connection_generation),
            rpc_transport,
            generation: connection_generation.load(AtomicOrdering::Acquire),
        }
    }

    fn is_standalone(&self) -> bool {
        matches!(&self.target, ClientDispatchTarget::Standalone)
    }

    fn generation_is_current(&self) -> bool {
        self.generation != 0
            && self.connection_generation.load(AtomicOrdering::Acquire) == self.generation
    }

    fn rpc_generation_is_live(&self) -> bool {
        self.generation_is_current()
            && self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                == self.generation
    }

    /// Revoke this reader's transport before any of its pending RPCs are
    /// completed with a retirement error.
    ///
    /// This closes admission immediately at the first terminal I/O/protocol
    /// result. The reconnect loop later drains the now-stable queue and mints
    /// the concrete successor authority. Repeating this for the same retired
    /// authority is intentionally idempotent so cleanup cannot reopen a race.
    fn begin_rpc_transport_retirement(&self) -> anyhow::Result<NonZeroU64> {
        let current = NonZeroU64::new(self.generation)
            .ok_or_else(|| anyhow!("cannot retire zero mux client connection generation"))?;
        let mut lifecycle = self.rpc_transport.lifecycle.lock();
        let next_generation = self.generation.checked_add(1).and_then(NonZeroU64::new);

        if let Some(error) = &lifecycle.terminal_error {
            return Err(anyhow::Error::new(error.clone()));
        }
        if let (Some(next_generation), RpcTransportPhase::Reconnecting { retired, next }) =
            (next_generation, lifecycle.phase)
        {
            if retired == current && next == next_generation {
                return Ok(next);
            }
        }
        if !matches!(lifecycle.phase, RpcTransportPhase::Live(observed) if observed == current) {
            bail!(
                "cannot retire mux client RPC generation {} from phase {:?}",
                current,
                lifecycle.phase
            );
        }
        let observed_connection_generation =
            self.connection_generation.load(AtomicOrdering::Acquire);
        if observed_connection_generation != self.generation {
            drop(lifecycle);
            let error = self.rpc_transport.mark_incarnation_terminal(
                RpcTransportError::ConnectionGenerationDiverged {
                    retiring_generation: current,
                    expected_generation: current,
                    observed_generation: observed_connection_generation,
                },
            );
            record_rpc_transport_error(&error);
            return Err(anyhow::Error::new(error));
        }

        let Some(next_generation) = next_generation else {
            drop(lifecycle);
            let error = self.rpc_transport.mark_incarnation_terminal(
                RpcTransportError::ConnectionGenerationExhausted {
                    last_generation: current,
                },
            );
            record_rpc_transport_error(&error);
            return Err(anyhow::Error::new(error));
        };

        self.rpc_transport
            .ready_generation
            .store(0, AtomicOrdering::Release);
        self.rpc_transport
            .live_generation
            .store(0, AtomicOrdering::Release);
        lifecycle.readiness_authority.retire();
        lifecycle.render_connection_identity = None;
        lifecycle.protocol = None;
        lifecycle.phase = RpcTransportPhase::Reconnecting {
            retired: current,
            next: next_generation,
        };
        Ok(next_generation)
    }

    /// Drain the retired generation's stable queue and mint its successor.
    fn advance_generation(&self, receiver: &Receiver<ReaderMessage>) -> anyhow::Result<Self> {
        let next_generation = match self.begin_rpc_transport_retirement() {
            Ok(next) => next,
            Err(error) => {
                receiver.close();
                while let Ok(message) = receiver.try_recv() {
                    message.retire(
                        &self.rpc_transport,
                        RpcRetirementStage::Queued,
                        "connection generation space exhausted or became inconsistent",
                    );
                }
                return Err(error);
            }
        };

        // Admission and new consumer commits are already disabled, so the
        // request queue is stable. Do not complete callers or record metrics
        // while holding the lifecycle lock: either action may wake arbitrary
        // executor work that immediately re-enters the transport.
        {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Reconnecting { retired, next }
                    if retired.get() == self.generation && next == next_generation
            ) {
                bail!(
                    "cannot drain mux client RPC generation {} from phase {:?}",
                    self.generation,
                    lifecycle.phase
                );
            }
        }
        while let Ok(message) = receiver.try_recv() {
            message.retire(
                &self.rpc_transport,
                RpcRetirementStage::Queued,
                "request remained queued when its transport retired",
            );
        }

        // Wait without spinning until every already-admitted synchronous
        // consumer commit finishes. Only then may the successor generation
        // become observable.
        let mut lifecycle = self.rpc_transport.lifecycle.lock();
        if !matches!(
            lifecycle.phase,
            RpcTransportPhase::Reconnecting { retired, next }
                if retired.get() == self.generation && next == next_generation
        ) {
            bail!(
                "cannot wait for mux client RPC generation {} from phase {:?}",
                self.generation,
                lifecycle.phase
            );
        }
        while lifecycle.active_consumer_commits != 0 {
            self.rpc_transport
                .consumer_commits_drained
                .wait(&mut lifecycle);
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Reconnecting { retired, next }
                    if retired.get() == self.generation && next == next_generation
            ) {
                bail!(
                    "mux client RPC generation {} changed phase while draining consumer commits: \
                     {:?}",
                    self.generation,
                    lifecycle.phase
                );
            }
        }
        if let Err(observed) = self.connection_generation.compare_exchange(
            self.generation,
            next_generation.get(),
            AtomicOrdering::AcqRel,
            AtomicOrdering::Acquire,
        ) {
            drop(lifecycle);
            let error = self.rpc_transport.mark_incarnation_terminal(
                RpcTransportError::ConnectionGenerationDiverged {
                    retiring_generation: NonZeroU64::new(self.generation)
                        .expect("retiring generation is nonzero"),
                    expected_generation: NonZeroU64::new(self.generation)
                        .expect("retiring generation is nonzero"),
                    observed_generation: observed,
                },
            );
            record_rpc_transport_error(&error);
            receiver.close();
            while let Ok(message) = receiver.try_recv() {
                message.retire(
                    &self.rpc_transport,
                    RpcRetirementStage::Queued,
                    "connection generation diverged while publishing its successor",
                );
            }
            return Err(anyhow::Error::new(error));
        }
        drop(lifecycle);

        let mut next = self.clone();
        next.generation = next_generation.get();
        Ok(next)
    }

    fn activate_rpc_transport(&self) -> anyhow::Result<()> {
        let generation = NonZeroU64::new(self.generation)
            .ok_or_else(|| anyhow!("cannot activate zero mux client connection generation"))?;
        let readiness_authority = Arc::new(RpcReadinessAuthority::new(generation));
        let reader_abort = Arc::new(RpcGenerationReaderAbortAuthority::new(generation));
        let mut lifecycle = self.rpc_transport.lifecycle.lock();
        match lifecycle.phase {
            RpcTransportPhase::Reconnecting { next, .. }
                if next == generation
                    && lifecycle.active_consumer_commits == 0
                    && lifecycle.terminal_error.is_none()
                    && self.connection_generation.load(AtomicOrdering::Acquire)
                        == generation.get() =>
            {
                lifecycle.phase = RpcTransportPhase::Live(generation);
                lifecycle.protocol = Some(RpcProtocolAuthority::new(generation));
                lifecycle.readiness_authority = readiness_authority;
                lifecycle.reader_abort = reader_abort;
                lifecycle.render_connection_identity = None;
                self.rpc_transport
                    .ready_generation
                    .store(0, AtomicOrdering::Release);
                self.rpc_transport
                    .live_generation
                    .store(generation.get(), AtomicOrdering::Release);
                Ok(())
            }
            observed => bail!(
                "cannot activate mux client RPC generation {} from phase {:?}",
                generation,
                observed
            ),
        }
    }

    fn close_rpc_transport(&self, receiver: &Receiver<ReaderMessage>, reason: &str) {
        {
            let mut lifecycle = self.rpc_transport.lifecycle.lock();
            self.rpc_transport
                .ready_generation
                .store(0, AtomicOrdering::Release);
            self.rpc_transport
                .live_generation
                .store(0, AtomicOrdering::Release);
            let last_live = match lifecycle.phase {
                RpcTransportPhase::Live(generation) => generation,
                RpcTransportPhase::Reconnecting { retired, .. } => retired,
                RpcTransportPhase::Closed { last_live } => last_live,
            };
            lifecycle.readiness_authority.retire();
            lifecycle.render_connection_identity = None;
            lifecycle.protocol = None;
            lifecycle.phase = RpcTransportPhase::Closed { last_live };
        }
        receiver.close();
        while let Ok(message) = receiver.try_recv() {
            message.retire(&self.rpc_transport, RpcRetirementStage::Queued, reason);
        }
        let mut lifecycle = self.rpc_transport.lifecycle.lock();
        while lifecycle.active_consumer_commits != 0 {
            self.rpc_transport
                .consumer_commits_drained
                .wait(&mut lifecycle);
        }
    }

    fn close_rpc_transport_without_receiver(&self) {
        let mut lifecycle = self.rpc_transport.lifecycle.lock();
        self.rpc_transport
            .ready_generation
            .store(0, AtomicOrdering::Release);
        self.rpc_transport
            .live_generation
            .store(0, AtomicOrdering::Release);
        let last_live = match lifecycle.phase {
            RpcTransportPhase::Live(generation) => generation,
            RpcTransportPhase::Reconnecting { retired, .. } => retired,
            RpcTransportPhase::Closed { last_live } => last_live,
        };
        lifecycle.readiness_authority.retire();
        lifecycle.render_connection_identity = None;
        lifecycle.protocol = None;
        lifecycle.phase = RpcTransportPhase::Closed { last_live };
        while lifecycle.active_consumer_commits != 0 {
            self.rpc_transport
                .consumer_commits_drained
                .wait(&mut lifecycle);
        }
    }

    fn captured_mux(&self) -> Option<Arc<Mux>> {
        match &self.target {
            ClientDispatchTarget::Standalone => None,
            ClientDispatchTarget::Attached { mux_owner, .. } => mux_owner.upgrade(),
        }
    }

    fn resolve_current(&self) -> anyhow::Result<Option<CurrentClientDispatch>> {
        if !self.generation_is_current() {
            return Ok(None);
        }
        let ClientDispatchTarget::Attached {
            local_domain_id, ..
        } = &self.target
        else {
            return Ok(None);
        };
        let Some(mux) = self.captured_mux() else {
            return Ok(None);
        };
        let Some(domain) = mux.get_domain(*local_domain_id) else {
            return Ok(None);
        };
        let client_domain = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain instance", local_domain_id))?;
        let Some(inner) = client_domain.inner() else {
            return Ok(None);
        };
        if inner.is_detached() || !inner.client.matches_dispatch_authority(self) {
            return Ok(None);
        }

        let current = CurrentClientDispatch {
            authority: self.clone(),
            mux,
            domain,
            inner,
        };
        Ok(current.is_current().then_some(current))
    }
}

impl CurrentClientDispatch {
    fn local_domain_id(&self) -> DomainId {
        self.domain.domain_id()
    }

    fn client_domain(&self) -> &ClientDomain {
        self.domain
            .downcast_ref::<ClientDomain>()
            .expect("current client dispatch was resolved from a ClientDomain")
    }

    pub(crate) fn bootstrap_rpc_scope(&self) -> RpcGenerationScope {
        let generation = NonZeroU64::new(self.authority.generation)
            .expect("current dispatch authority generation is nonzero");
        self.inner.client.bootstrap_rpc_scope_at(generation)
    }

    fn commit_sync<T>(
        &self,
        consumer: RpcConsumerKind,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.bootstrap_rpc_scope()
            .commit_sync(consumer, commit)
            .map_err(anyhow::Error::new)?
    }

    fn is_current(&self) -> bool {
        if !self.authority.generation_is_current() || self.inner.is_detached() {
            return false;
        }
        let ClientDispatchTarget::Attached {
            local_domain_id,
            mux_owner,
        } = &self.authority.target
        else {
            return false;
        };
        if *local_domain_id != self.domain.domain_id()
            || !mux_owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.mux))
            || !self
                .mux
                .get_domain(*local_domain_id)
                .is_some_and(|current| current.same_registration(&self.domain))
        {
            return false;
        }

        self.client_domain().inner_is_current(&self.inner)
            && self
                .inner
                .client
                .matches_dispatch_authority(&self.authority)
    }

    fn rpc_generation_is_live(&self) -> bool {
        self.is_current() && self.authority.rpc_generation_is_live()
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Codec version mismatch: local window {}..={} (frankenterm {}), remote window \
     {remote_min_supported}..={codec_vers} (frankenterm {version}). The peers' advertised \
     codec compatibility windows do not overlap. Stage the exact same-release ft + \
     frankenterm-mux-server process family on the remote, drain its live PTYs, then restart \
     that mux and retry. If those PTYs cannot be drained yet, roll back the desktop client \
     to the last release whose codec window includes the remote server; no automatic mux \
     restart was attempted. See docs/codec-atomic-redeploy.md for the server-first deploy \
     and rollback runbook.",
    codec::CODEC_VERSION_MIN_SUPPORTED,
    CODEC_VERSION,
    config::wezterm_version()
)]
pub struct IncompatibleVersionError {
    pub version: String,
    pub codec_vers: usize,
    pub remote_min_supported: usize,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Remote FrankenTerm codec version {remote_codec_version} predates the required \
     generation-fenced topology protocol (minimum {minimum_codec_version}); mixed-version \
     attachment is rejected before any topology snapshot can mutate local mux state"
)]
pub struct MissingTopologyFenceProtocolError {
    pub remote_codec_version: usize,
    pub minimum_codec_version: usize,
}

fn remote_rejection_error(
    method: &'static str,
    expected_request_ident: u64,
    response: &ErrorResponse,
) -> anyhow::Error {
    const CLI_MUX_ERROR_V1_PREFIX: &str = "FRANKENTERM_MUX_ERROR_V1";

    if response.request_ident != expected_request_ident || response.validate().is_err() {
        return anyhow!(
            "{CLI_MUX_ERROR_V1_PREFIX} request_ident={} response_request_ident={} operation={method} code=unknown_future object=none effect=unknown_future retry=unknown_future",
            expected_request_ident,
            response.request_ident
        );
    }
    let object = response.object.map_or_else(
        || "none".to_string(),
        |object| format!("{}:{}", object.kind.label(), object.id),
    );
    anyhow!(
        "{CLI_MUX_ERROR_V1_PREFIX} request_ident={} response_request_ident={} operation={method} code={} object={} effect={} retry={}",
        expected_request_ident,
        response.request_ident,
        response.code.label(),
        object,
        response.effect.label(),
        response.retry.label()
    )
}

struct RpcAttemptMetricGuard {
    method: &'static str,
    started: std::time::Instant,
    outcome: &'static str,
    active: metrics::Gauge,
}

impl RpcAttemptMetricGuard {
    fn new(method: &'static str) -> Self {
        metrics::counter!("rpc.attempt.total", "method" => method).increment(1);
        let active = metrics::gauge!("rpc.active", "method" => method);
        active.increment(1);
        Self {
            method,
            started: std::time::Instant::now(),
            outcome: "abandoned",
            active,
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

impl Drop for RpcAttemptMetricGuard {
    fn drop(&mut self) {
        self.active.decrement(1);
        metrics::histogram!("rpc", "method" => self.method).record(self.started.elapsed());
        metrics::counter!("rpc.count", "method" => self.method).increment(1);
        metrics::counter!(
            "rpc.outcome.total",
            "method" => self.method,
            "outcome" => self.outcome,
        )
        .increment(1);
    }
}

macro_rules! rpc {
    ($method_name:ident, $request_type:ident, $response_type:ident) => {
        pub fn $method_name(
            &self,
            pdu: $request_type,
        ) -> impl std::future::Future<Output = anyhow::Result<$response_type>> + Send + 'static {
            let mut metric_guard = RpcAttemptMetricGuard::new(stringify!($method_name));
            let request_ident = <$request_type as PduWireIdent>::IDENT;
            // `send_pdu` binds synchronously here, before this future can be
            // moved into a detached task and first polled on a later transport.
            let request = self.send_pdu_expect(
                Pdu::$request_type(pdu),
                Some(
                    NonZeroU64::new(<$response_type as PduWireIdent>::IDENT)
                        .expect("typed success response must have a nonzero wire identifier"),
                ),
            );
            async move {
                let result = request.await;
                match result {
                    Ok(Pdu::$response_type(res)) => {
                        metric_guard.finish("success");
                        Ok(res)
                    }
                    Ok(Pdu::ErrorResponse(err)) => {
                        metric_guard.finish("remote_error");
                        Err(remote_rejection_error(
                            stringify!($method_name),
                            request_ident,
                            &err,
                        ))
                    }
                    Ok(other) => {
                        metric_guard.finish("unexpected_response");
                        Err(anyhow!(
                            "unexpected {} response to {}; expected {}",
                            other.pdu_name(),
                            stringify!($method_name),
                            stringify!($response_type)
                        ))
                    }
                    Err(err) => {
                        metric_guard.finish("transport_error");
                        Err(err)
                    }
                }
            }
        }
    };

    // This variant allows omitting the request parameter; this is useful
    // in the case where the struct is empty and present only for the purpose
    // of typing the request.
    ($method_name:ident, $request_type:ident=(), $response_type:ident) => {
        #[allow(dead_code)]
        pub fn $method_name(
            &self,
        ) -> impl std::future::Future<Output = anyhow::Result<$response_type>> + Send + 'static {
            let mut metric_guard = RpcAttemptMetricGuard::new(stringify!($method_name));
            let request_ident = <$request_type as PduWireIdent>::IDENT;
            let request = self.send_pdu_expect(
                Pdu::$request_type($request_type {}),
                Some(
                    NonZeroU64::new(<$response_type as PduWireIdent>::IDENT)
                        .expect("typed success response must have a nonzero wire identifier"),
                ),
            );
            async move {
                let result = request.await;
                match result {
                    Ok(Pdu::$response_type(res)) => {
                        metric_guard.finish("success");
                        Ok(res)
                    }
                    Ok(Pdu::ErrorResponse(err)) => {
                        metric_guard.finish("remote_error");
                        Err(remote_rejection_error(
                            stringify!($method_name),
                            request_ident,
                            &err,
                        ))
                    }
                    Ok(other) => {
                        metric_guard.finish("unexpected_response");
                        Err(anyhow!(
                            "unexpected {} response to {}; expected {}",
                            other.pdu_name(),
                            stringify!($method_name),
                            stringify!($response_type)
                        ))
                    }
                    Err(err) => {
                        metric_guard.finish("transport_error");
                        Err(err)
                    }
                }
            }
        }
    };
}

macro_rules! rpc_surface {
    () => {
        rpc!(ping, Ping = (), Pong);
        rpc!(list_panes, ListPanes = (), ListPanesResponse);
        rpc!(
            list_panes_coherent,
            ListPanesCoherent,
            ListPanesCoherentResponse
        );
        rpc!(spawn_v2, SpawnV2, SpawnResponse);
        rpc!(split_pane, SplitPane, SpawnResponse);
        rpc!(
            move_pane_to_new_tab,
            MovePaneToNewTab,
            MovePaneToNewTabResponse
        );
        rpc!(write_to_pane, WriteToPane, UnitResponse);
        rpc!(
            reliable_pane_write_v1,
            ReliablePaneWriteV1,
            ReliablePaneWriteV1Response
        );
        rpc!(send_paste, SendPaste, UnitResponse);
        rpc!(send_paste_traced_v1, SendPasteTracedV1, UnitResponse);
        rpc!(key_down, SendKeyDown, UnitResponse);
        rpc!(key_up, SendKeyUp, UnitResponse);
        rpc!(
            reliable_key_event_v1,
            ReliableKeyEventV1,
            ReliableKeyEventV1Response
        );
        rpc!(
            reliable_key_event_traced_v1,
            ReliableKeyEventTracedV1,
            ReliableKeyEventV1Response
        );
        rpc!(mouse_event, SendMouseEvent, UnitResponse);
        rpc!(resize, Resize, UnitResponse);
        rpc!(set_zoomed, SetPaneZoomed, UnitResponse);
        rpc!(activate_pane_direction, ActivatePaneDirection, UnitResponse);
        rpc!(
            get_pane_render_changes,
            GetPaneRenderChanges,
            LivenessResponse
        );
        rpc!(get_lines, GetLines, GetLinesResponse);
        rpc!(
            get_dimensions,
            GetPaneRenderableDimensions,
            GetPaneRenderableDimensionsResponse
        );
        rpc!(get_codec_version, GetCodecVersion, GetCodecVersionResponse);
        rpc!(get_tls_creds, GetTlsCreds = (), GetTlsCredsResponse);
        rpc!(
            search_scrollback,
            SearchScrollbackRequest,
            SearchScrollbackResponse
        );
        rpc!(kill_pane, KillPane, UnitResponse);
        rpc!(set_client_id, SetClientId, UnitResponse);
        rpc!(list_clients, GetClientList = (), GetClientListResponse);
        rpc!(set_window_workspace, SetWindowWorkspace, UnitResponse);
        rpc!(set_active_workspace, SetActiveWorkspace, UnitResponse);
        rpc!(set_focused_pane_id, SetFocusedPane, UnitResponse);
        rpc!(get_image_cell, GetImageCell, GetImageCellResponse);
        rpc!(set_configured_palette_for_pane, SetPalette, UnitResponse);
        rpc!(set_tab_title, TabTitleChanged, UnitResponse);
        rpc!(set_window_title, WindowTitleChanged, UnitResponse);
        rpc!(rename_workspace, RenameWorkspace, UnitResponse);
        rpc!(erase_scrollback, EraseScrollbackRequest, UnitResponse);
        rpc!(
            get_pane_direction,
            GetPaneDirection,
            GetPaneDirectionResponse
        );
        rpc!(adjust_pane_size, AdjustPaneSize, UnitResponse);
    };
}

fn admit_client_pane(
    dispatch: &CurrentClientDispatch,
    remote_pane_id: PaneId,
) -> Option<(Arc<dyn Pane>, PaneRegistrationHandle)> {
    if !dispatch.rpc_generation_is_live() {
        return None;
    }
    let local_pane_id = dispatch
        .inner
        .remote_to_local_pane_id(&dispatch.mux, remote_pane_id)?;
    let pane = dispatch.mux.get_pane(local_pane_id)?;
    let client_pane = pane.downcast_ref::<ClientPane>()?;
    if !client_pane.belongs_to_client(&dispatch.inner)
        || client_pane.remote_pane_id() != remote_pane_id
    {
        return None;
    }
    let registration = dispatch.mux.capture_pane_registration(&pane)?;
    dispatch
        .rpc_generation_is_live()
        .then_some((pane, registration))
}

async fn process_unilateral_inner_async(
    dispatch: CurrentClientDispatch,
    admitted: Option<(Arc<dyn Pane>, PaneRegistrationHandle)>,
    pane_id: PaneId,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    let local_domain_id = dispatch.local_domain_id();
    if !dispatch.rpc_generation_is_live() {
        log::trace!(
            "discarding unilateral PDU for retired client connection in domain {}",
            local_domain_id
        );
        return Ok(());
    }
    let client_domain = dispatch.client_domain();
    let rpc = dispatch.bootstrap_rpc_scope();

    let (pane, registration) = if let Some(admitted) = admitted {
        admitted
    } else {
        // If we get a push for a pane that we don't yet know about, it means
        // that some other client has manipulated the mux topology; re-sync on
        // the captured origin, never a later process-global replacement.
        let local_pane_id = match dispatch
            .inner
            .remote_to_local_pane_id(&dispatch.mux, pane_id)
        {
            Some(p) => p,
            None => {
                log::debug!("got {decoded:?}, pane not found locally, resync");
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let resync_result = client_domain
                    .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner), &rpc)
                    .await;
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let _ = resync_result?;
                dispatch
                    .inner
                    .remote_to_local_pane_id(&dispatch.mux, pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?
            }
        };

        let pane = match dispatch.mux.get_pane(local_pane_id) {
            Some(p) => p,
            None => {
                log::debug!(
                    "got {decoded:?}, but local pane {local_pane_id} no longer exists; resync"
                );
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let resync_result = client_domain
                    .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner), &rpc)
                    .await;
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let _ = resync_result?;

                let local_pane_id = dispatch
                    .inner
                    .remote_to_local_pane_id(&dispatch.mux, pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?;

                dispatch
                    .mux
                    .get_pane(local_pane_id)
                    .ok_or_else(|| anyhow!("local pane {local_pane_id} not found"))?
            }
        };
        if !dispatch.rpc_generation_is_live() {
            return Ok(());
        }
        let registration = dispatch
            .mux
            .capture_pane_registration(&pane)
            .ok_or_else(|| anyhow!("local pane {} is no longer registered", pane.pane_id()))?;
        (pane, registration)
    };
    let local_pane_id = pane.pane_id();
    let client_pane = pane.downcast_ref::<ClientPane>().ok_or_else(|| {
        log::error!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        );
        anyhow!(
            "received unilateral PDU for pane {} which is \
                     not an instance of ClientPane: {:?}",
            local_pane_id,
            decoded.pdu
        )
    })?;
    if !dispatch.rpc_generation_is_live()
        || !client_pane.belongs_to_client(&dispatch.inner)
        || client_pane.remote_pane_id() != pane_id
    {
        log::trace!(
            "discarding unilateral PDU for stale client pane {} (remote {})",
            local_pane_id,
            pane_id
        );
        return Ok(());
    }
    let result = client_pane
        .process_unilateral(&registration, &rpc, decoded.pdu)
        .await;
    if !dispatch.rpc_generation_is_live() {
        return Ok(());
    }
    result?;
    Ok(())
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum StandaloneUnilateralError {
    #[error(
        "standalone mux client cannot process {pdu_name} unilateral PDU without a local domain; \
         reconnect with an attached client domain or retry after topology settles"
    )]
    RequiresAttachedDomain { pdu_name: &'static str },
}

fn unilateral_without_local_domain_is_ignorable(pdu: &Pdu) -> bool {
    match pdu {
        Pdu::WindowTitleChanged(_)
        | Pdu::TabTitleChanged(_)
        | Pdu::PaneFocused(_)
        | Pdu::TabResized(_)
        | Pdu::NotifyAlert(_)
        | Pdu::SetClipboard(_) => true,
        Pdu::PaneRemoved(_)
        | Pdu::TabAddedToWindow(_)
        | Pdu::WindowWorkspaceChanged(_)
        | Pdu::RenameWorkspace(_) => false,
        _ => pdu.pane_id().is_some(),
    }
}

fn handle_unilateral_without_local_domain(decoded: &DecodedPdu) -> anyhow::Result<()> {
    if unilateral_without_local_domain_is_ignorable(&decoded.pdu) {
        log::trace!(
            "standalone mux client explicitly ignored {} unilateral PDU",
            decoded.pdu.pdu_name()
        );
        return Ok(());
    }

    Err(StandaloneUnilateralError::RequiresAttachedDomain {
        pdu_name: decoded.pdu.pdu_name(),
    }
    .into())
}

fn process_unilateral(
    authority: &ClientDispatchAuthority,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    if !authority.rpc_generation_is_live() {
        return Ok(());
    }
    if authority.is_standalone() {
        return handle_unilateral_without_local_domain(&decoded);
    }
    let Some(dispatch) = authority.resolve_current()? else {
        return Ok(());
    };
    reserve_client_main_thread(
        MainThreadServiceClass::Topology,
        CLIENT_MAIN_THREAD_TOPOLOGY_ESTIMATED_BYTES,
        "unilateral client update",
    )?
    .spawn(async move { apply_unilateral_on_main_thread(dispatch, decoded).await })
    .detach();
    Ok(())
}

async fn process_unilateral_with_barrier(
    authority: &ClientDispatchAuthority,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    if !authority.rpc_generation_is_live() {
        return Ok(());
    }
    if authority.is_standalone() {
        handle_unilateral_without_local_domain(&decoded)?;
        return Ok(());
    }
    let Some(dispatch) = authority.resolve_current()? else {
        return Ok(());
    };
    reserve_client_main_thread(
        MainThreadServiceClass::Topology,
        CLIENT_MAIN_THREAD_TOPOLOGY_ESTIMATED_BYTES,
        "barriered unilateral client update",
    )?
    .spawn(async move { apply_unilateral_on_main_thread(dispatch, decoded).await })
    .into_task()
    .await
}

async fn apply_unilateral_on_main_thread(
    dispatch: CurrentClientDispatch,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    match &decoded.pdu {
        Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
            window_id,
            workspace,
        }) => {
            let window_id = *window_id;
            let workspace = workspace.to_string();
            return dispatch.commit_sync(RpcConsumerKind::GlobalUnilateral, || {
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let local_window_id = dispatch
                    .client_domain()
                    .remote_to_local_window_id(window_id)
                    .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
                let _remote_application = dispatch.inner.begin_remote_metadata_application()?;
                dispatch
                    .mux
                    .set_window_workspace(local_window_id, &workspace)?;
                Ok(())
            });
        }
        Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
            let title = title.to_string();
            let window_id = *window_id;
            return dispatch.commit_sync(RpcConsumerKind::GlobalUnilateral, || {
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let local_window_id = dispatch
                    .client_domain()
                    .remote_to_local_window_id(window_id)
                    .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
                let _remote_application = dispatch.inner.begin_remote_metadata_application()?;
                dispatch.mux.set_window_title(local_window_id, &title)?;
                Ok(())
            });
        }
        Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace,
            new_workspace,
        }) => {
            let old_workspace = old_workspace.to_string();
            let new_workspace = new_workspace.to_string();
            return dispatch.commit_sync(RpcConsumerKind::GlobalUnilateral, || {
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                log::debug!("got a rename {old_workspace} -> {new_workspace}");
                let _remote_application = dispatch.inner.begin_remote_metadata_application()?;
                dispatch
                    .mux
                    .rename_workspace(&old_workspace, &new_workspace)?;
                Ok(())
            });
        }
        Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
            let title = title.to_string();
            let tab_id = *tab_id;
            return dispatch.commit_sync(RpcConsumerKind::GlobalUnilateral, || {
                if !dispatch.rpc_generation_is_live() {
                    return Ok(());
                }
                let local_tab_id = dispatch
                    .inner
                    .remote_to_local_tab_id(tab_id)
                    .ok_or_else(|| anyhow!("no local tab for remote tab id {}", tab_id))?;
                let _remote_application = dispatch.inner.begin_remote_metadata_application()?;
                dispatch.mux.set_tab_title(local_tab_id, &title);
                Ok(())
            });
        }
        Pdu::TabResized(_) | Pdu::TabAddedToWindow(_) => {
            log::trace!("resync due to {:?}", decoded.pdu);
            if !dispatch.rpc_generation_is_live() {
                return Ok(());
            }
            let rpc = dispatch.bootstrap_rpc_scope();
            let result = dispatch
                .client_domain()
                .resync_if_current(Arc::clone(&dispatch.mux), Arc::clone(&dispatch.inner), &rpc)
                .await;
            if !dispatch.rpc_generation_is_live() {
                return Ok(());
            }
            let _ = result?;
            return Ok(());
        }
        _ => {}
    }

    if let Some(pane_id) = decoded.pdu.pane_id() {
        if !dispatch.rpc_generation_is_live() {
            return Ok(());
        }
        let admitted = admit_client_pane(&dispatch, pane_id);
        if !dispatch.rpc_generation_is_live() {
            return Ok(());
        }
        process_unilateral_inner_async(dispatch, admitted, pane_id, decoded).await
    } else {
        bail!("don't know how to handle {:?}", decoded);
    }
}

#[derive(Debug)]
struct QuarantinedUnilateral {
    frame: Vec<u8>,
}

impl QuarantinedUnilateral {
    fn encode(decoded: DecodedPdu) -> anyhow::Result<Self> {
        debug_assert_eq!(decoded.serial, 0);
        let frame = decoded
            .pdu
            .encode_retained_frame(0)
            .context("encoding a pre-ready unilateral PDU for bounded retention")?;
        Ok(Self { frame })
    }

    fn retained_bytes(&self) -> usize {
        self.frame.len()
    }

    fn decode(self) -> anyhow::Result<DecodedPdu> {
        let decoded = Pdu::decode_retained_frame(self.frame.as_slice())
            .context("decoding a retained pre-ready unilateral PDU")?;
        if decoded.serial != 0 {
            bail!(
                "retained unilateral PDU replay decoded reserved serial {}",
                decoded.serial
            );
        }
        Ok(decoded)
    }
}

#[derive(Debug, Default)]
struct PreReadyUnilateralQueue {
    waiting: VecDeque<QuarantinedUnilateral>,
    waiting_bytes: usize,
}

impl PreReadyUnilateralQueue {
    fn enqueue(
        &mut self,
        decoded: DecodedPdu,
        replayed_pdus_in_flight: usize,
        replayed_bytes_in_flight: usize,
    ) -> anyhow::Result<()> {
        self.enqueue_with_limits(
            decoded,
            replayed_pdus_in_flight,
            replayed_bytes_in_flight,
            MAX_PRE_READY_UNILATERAL_PDUS,
            MAX_PRE_READY_UNILATERAL_BYTES,
        )
    }

    fn enqueue_with_limits(
        &mut self,
        decoded: DecodedPdu,
        replayed_pdus_in_flight: usize,
        replayed_bytes_in_flight: usize,
        max_pdus: usize,
        max_bytes: usize,
    ) -> anyhow::Result<()> {
        let retained_pdus = self
            .waiting
            .len()
            .checked_add(replayed_pdus_in_flight)
            .context("pre-ready unilateral PDU count overflow")?;
        if retained_pdus >= max_pdus {
            metrics::counter!(
                "mux.client.rpc.pre_ready_quarantine_rejected.total",
                "reason" => "count_limit"
            )
            .increment(1);
            bail!(
                "pre-ready unilateral quarantine reached its {} PDU limit",
                max_pdus
            );
        }

        let queued = QuarantinedUnilateral::encode(decoded)?;
        let retained_bytes = self
            .waiting_bytes
            .checked_add(replayed_bytes_in_flight)
            .context("pre-ready unilateral retained-byte count overflow")?;
        let total_bytes = retained_bytes
            .checked_add(queued.retained_bytes())
            .context("pre-ready unilateral retained-byte count overflow")?;
        if total_bytes > max_bytes {
            metrics::counter!(
                "mux.client.rpc.pre_ready_quarantine_rejected.total",
                "reason" => "byte_limit"
            )
            .increment(1);
            bail!(
                "pre-ready unilateral quarantine would retain {} bytes, above its {} byte limit",
                total_bytes,
                max_bytes
            );
        }
        self.waiting
            .try_reserve(1)
            .context("reserving the bounded pre-ready unilateral queue")?;
        self.waiting_bytes = self
            .waiting_bytes
            .checked_add(queued.retained_bytes())
            .context("pre-ready unilateral waiting-byte count overflow")?;
        self.waiting.push_back(queued);
        Ok(())
    }

    fn take_batch(&mut self) -> anyhow::Result<(VecDeque<QuarantinedUnilateral>, usize)> {
        let bytes = self
            .waiting
            .front()
            .expect("pre-ready replay batch requires one waiting PDU")
            .retained_bytes();
        let remaining_bytes = self
            .waiting_bytes
            .checked_sub(bytes)
            .context("pre-ready unilateral batch byte accounting underflow")?;
        let queued = self
            .waiting
            .pop_front()
            .expect("the validated pre-ready replay PDU must still be present");
        self.waiting_bytes = remaining_bytes;
        let mut batch = VecDeque::with_capacity(1);
        batch.push_back(queued);
        Ok((batch, bytes))
    }

    fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    fn take_all(&mut self) -> VecDeque<QuarantinedUnilateral> {
        self.waiting_bytes = 0;
        std::mem::take(&mut self.waiting)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TopologyFenceAuthority {
    stream_id: TopologyStreamId,
    session_incarnation: MuxSessionIncarnation,
    snapshot_revision: TopologyRevision,
}

impl TopologyFenceAuthority {
    fn from_response(response: &ListPanesCoherentResponse) -> anyhow::Result<Self> {
        if response.negotiated != TopologyCapabilities::FENCED_SNAPSHOT_V1 {
            bail!(
                "coherent topology snapshot negotiated unexpected capability bits {:#x}",
                response.negotiated.bits()
            );
        }
        if response.stream_id.as_bytes() == [0; 16] {
            bail!("coherent topology snapshot carried the reserved zero stream identity");
        }
        let ListPanesCoherentOutcome::Snapshot(snapshot) = &response.outcome else {
            bail!("coherent topology authority requires a snapshot outcome");
        };
        if snapshot.session_incarnation.as_bytes() == [0; 16] {
            bail!("coherent topology snapshot carried the reserved zero session incarnation");
        }
        if snapshot.snapshot_revision.get() == u64::MAX {
            bail!("coherent topology snapshot used the exhausted terminal revision");
        }
        Ok(Self {
            stream_id: response.stream_id,
            session_incarnation: snapshot.session_incarnation,
            snapshot_revision: snapshot.snapshot_revision,
        })
    }

    const fn render_connection_identity(self) -> RenderConnectionIdentity {
        RenderConnectionIdentity::new(self.stream_id, self.session_incarnation)
    }
}

#[derive(Debug)]
struct RetainedClientTopologyEvent {
    event: TopologyEvent,
    retained_bytes: usize,
}

impl RetainedClientTopologyEvent {
    fn new(event: TopologyEvent) -> anyhow::Result<Self> {
        let retained_bytes = Pdu::TopologyEvent(event.clone())
            .encode_retained_frame(0)
            .context("encoding a topology event for bounded client retention")?
            .len();
        Ok(Self {
            event,
            retained_bytes,
        })
    }
}

#[derive(Debug, Default)]
struct ClientTopologyEventBuffer {
    events: BTreeMap<TopologyRevision, RetainedClientTopologyEvent>,
    retained_bytes: usize,
}

impl ClientTopologyEventBuffer {
    fn insert(&mut self, event: TopologyEvent) -> anyhow::Result<()> {
        self.insert_with_limits(event, MAX_TOPOLOGY_FENCE_EVENTS, MAX_TOPOLOGY_FENCE_BYTES)
    }

    fn insert_with_limits(
        &mut self,
        event: TopologyEvent,
        max_events: usize,
        max_bytes: usize,
    ) -> anyhow::Result<()> {
        if self.events.contains_key(&event.revision) {
            bail!(
                "duplicate client topology revision {}",
                event.revision.get()
            );
        }
        let retained = RetainedClientTopologyEvent::new(event)?;
        let next_len = self
            .events
            .len()
            .checked_add(1)
            .context("counting retained client topology events")?;
        let next_bytes = self
            .retained_bytes
            .checked_add(retained.retained_bytes)
            .context("counting retained client topology bytes")?;
        if next_len > max_events || next_bytes > max_bytes {
            bail!(
                "client topology fence would retain {next_len} events and {next_bytes} bytes \
                 above limits of {max_events} events and {max_bytes} bytes"
            );
        }
        self.retained_bytes = next_bytes;
        self.events.insert(retained.event.revision, retained);
        metrics::histogram!("mux.client.topology_fence.retained_events").record(next_len as f64);
        metrics::histogram!("mux.client.topology_fence.retained_bytes").record(next_bytes as f64);
        Ok(())
    }

    fn remove(&mut self, revision: TopologyRevision) -> anyhow::Result<Option<TopologyEvent>> {
        let Some(retained) = self.events.remove(&revision) else {
            return Ok(None);
        };
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(retained.retained_bytes)
            .context("decrementing retained client topology bytes")?;
        Ok(Some(retained.event))
    }

    fn take_all(&mut self) -> Vec<RetainedClientTopologyEvent> {
        self.retained_bytes = 0;
        std::mem::take(&mut self.events).into_values().collect()
    }
}

#[derive(Debug)]
enum ClientTopologyPrior {
    Legacy { buffered: PreReadyUnilateralQueue },
    Established(EstablishedClientTopologyStream),
}

#[derive(Debug)]
struct ClientTopologyFenceInFlight {
    serial: NonZeroU64,
    prior: ClientTopologyPrior,
}

#[derive(Debug)]
struct ClientTopologyAwaitingCommit {
    authority: TopologyFenceAuthority,
    legacy: PreReadyUnilateralQueue,
    events: ClientTopologyEventBuffer,
}

#[derive(Debug)]
struct ClientLegacyTopologyAwaitingCommit {
    serial: NonZeroU64,
    buffered: PreReadyUnilateralQueue,
}

#[derive(Debug)]
struct EstablishedClientTopologyStream {
    authority: TopologyFenceAuthority,
    next_revision: Option<TopologyRevision>,
    events: ClientTopologyEventBuffer,
}

#[derive(Debug, Default)]
enum ClientTopologyPhase {
    #[default]
    Legacy,
    Fencing(ClientTopologyFenceInFlight),
    AwaitingCommit(ClientTopologyAwaitingCommit),
    LegacyAwaitingCommit(ClientLegacyTopologyAwaitingCommit),
    Established(EstablishedClientTopologyStream),
    Closed,
}

#[derive(Debug, Default)]
struct ClientTopologyCoordinator {
    phase: ClientTopologyPhase,
}

#[derive(Debug)]
enum ClientTopologyResponseAction {
    AwaitCommit,
    Route(Vec<DecodedPdu>),
    TerminalAfterDelivery(&'static str),
}

#[derive(Debug)]
enum ClientTopologyUnilateralAction {
    Buffered,
    Route(Vec<DecodedPdu>),
}

impl ClientTopologyCoordinator {
    fn begin_legacy_fence(&mut self, serial: NonZeroU64) -> anyhow::Result<()> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        self.phase = match phase {
            ClientTopologyPhase::Legacy => {
                ClientTopologyPhase::Fencing(ClientTopologyFenceInFlight {
                    serial,
                    prior: ClientTopologyPrior::Legacy {
                        buffered: PreReadyUnilateralQueue::default(),
                    },
                })
            }
            other => {
                self.phase = other;
                bail!("overlapping or cross-dialect legacy topology snapshot request")
            }
        };
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "legacy_request_admitted"
        )
        .increment(1);
        Ok(())
    }

    fn begin_fence(&mut self, serial: NonZeroU64) -> anyhow::Result<()> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        self.phase = match phase {
            ClientTopologyPhase::Legacy => {
                ClientTopologyPhase::Fencing(ClientTopologyFenceInFlight {
                    serial,
                    prior: ClientTopologyPrior::Legacy {
                        buffered: PreReadyUnilateralQueue::default(),
                    },
                })
            }
            ClientTopologyPhase::Established(established) => {
                ClientTopologyPhase::Fencing(ClientTopologyFenceInFlight {
                    serial,
                    prior: ClientTopologyPrior::Established(established),
                })
            }
            ClientTopologyPhase::Fencing(in_flight) => {
                self.phase = ClientTopologyPhase::Fencing(in_flight);
                bail!("overlapping coherent topology snapshot requests")
            }
            ClientTopologyPhase::AwaitingCommit(awaiting) => {
                self.phase = ClientTopologyPhase::AwaitingCommit(awaiting);
                bail!("a coherent topology snapshot request preceded consumer commit")
            }
            ClientTopologyPhase::LegacyAwaitingCommit(awaiting) => {
                self.phase = ClientTopologyPhase::LegacyAwaitingCommit(awaiting);
                bail!("a legacy topology snapshot request preceded consumer commit")
            }
            ClientTopologyPhase::Closed => {
                self.phase = ClientTopologyPhase::Closed;
                bail!("client topology stream is already closed")
            }
        };
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "request_admitted"
        )
        .increment(1);
        Ok(())
    }

    fn on_unilateral(
        &mut self,
        decoded: DecodedPdu,
    ) -> anyhow::Result<ClientTopologyUnilateralAction> {
        if let Pdu::TopologyEvent(event) = &decoded.pdu {
            return self.on_topology_event(event.clone());
        }

        if !is_legacy_topology_pdu(&decoded.pdu) {
            return Ok(ClientTopologyUnilateralAction::Route(vec![decoded]));
        }

        match &mut self.phase {
            ClientTopologyPhase::Legacy => Ok(ClientTopologyUnilateralAction::Route(vec![decoded])),
            ClientTopologyPhase::Fencing(in_flight) => match &mut in_flight.prior {
                ClientTopologyPrior::Legacy { buffered } => {
                    buffered.enqueue_with_limits(
                        decoded,
                        0,
                        0,
                        MAX_TOPOLOGY_FENCE_EVENTS,
                        MAX_TOPOLOGY_FENCE_BYTES,
                    )?;
                    Ok(ClientTopologyUnilateralAction::Buffered)
                }
                ClientTopologyPrior::Established(_) => {
                    bail!("legacy topology PDU crossed an established stamped-stream fence")
                }
            },
            ClientTopologyPhase::LegacyAwaitingCommit(awaiting) => {
                awaiting.buffered.enqueue_with_limits(
                    decoded,
                    0,
                    0,
                    MAX_TOPOLOGY_FENCE_EVENTS,
                    MAX_TOPOLOGY_FENCE_BYTES,
                )?;
                Ok(ClientTopologyUnilateralAction::Buffered)
            }
            ClientTopologyPhase::AwaitingCommit(_) | ClientTopologyPhase::Established(_) => {
                bail!("legacy topology PDU arrived after fenced-stream negotiation")
            }
            ClientTopologyPhase::Closed => bail!("client topology stream is closed"),
        }
    }

    fn on_topology_event(
        &mut self,
        event: TopologyEvent,
    ) -> anyhow::Result<ClientTopologyUnilateralAction> {
        if event.stream_id.as_bytes() == [0; 16] {
            bail!("topology event carried the reserved zero stream identity");
        }
        if event.revision == TopologyRevision::INITIAL {
            bail!("topology event used the initial snapshot-only revision");
        }
        if event.revision.get() == u64::MAX {
            bail!("topology event used the exhausted terminal revision");
        }
        match &mut self.phase {
            ClientTopologyPhase::Legacy => {
                bail!("stamped topology event arrived before stream negotiation")
            }
            ClientTopologyPhase::Fencing(in_flight) => match &mut in_flight.prior {
                ClientTopologyPrior::Legacy { .. } => {
                    bail!("stamped topology event overtook its establishing snapshot")
                }
                ClientTopologyPrior::Established(established) => {
                    Self::retain_established_event(established, event)?;
                    Ok(ClientTopologyUnilateralAction::Buffered)
                }
            },
            ClientTopologyPhase::AwaitingCommit(awaiting) => {
                if event.stream_id != awaiting.authority.stream_id {
                    bail!("topology event carried the wrong stream identity before commit");
                }
                awaiting.events.insert(event)?;
                Ok(ClientTopologyUnilateralAction::Buffered)
            }
            ClientTopologyPhase::LegacyAwaitingCommit(_) => {
                bail!("stamped topology event arrived in a codec-46 snapshot gate")
            }
            ClientTopologyPhase::Established(established) => {
                Self::retain_established_event(established, event)?;
                Ok(ClientTopologyUnilateralAction::Route(
                    Self::drain_established(established)?,
                ))
            }
            ClientTopologyPhase::Closed => bail!("client topology stream is closed"),
        }
    }

    fn on_response(
        &mut self,
        serial: NonZeroU64,
        pdu: &Pdu,
    ) -> anyhow::Result<ClientTopologyResponseAction> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::Fencing(in_flight) = phase else {
            self.phase = phase;
            bail!("coherent topology response arrived without an active client fence");
        };
        if in_flight.serial != serial {
            bail!(
                "coherent topology response serial {} did not match active fence {}",
                serial,
                in_flight.serial
            );
        }

        match pdu {
            Pdu::ListPanesCoherentResponse(response) => match &response.outcome {
                ListPanesCoherentOutcome::Snapshot(_) => {
                    let authority = TopologyFenceAuthority::from_response(response)?;
                    let (legacy, events) = match in_flight.prior {
                        ClientTopologyPrior::Legacy { buffered } => {
                            (buffered, ClientTopologyEventBuffer::default())
                        }
                        ClientTopologyPrior::Established(established) => {
                            if established.authority.stream_id != authority.stream_id
                                || established.authority.session_incarnation
                                    != authority.session_incarnation
                            {
                                bail!(
                                    "coherent topology snapshot changed stream or session within \
                                    one connection generation"
                                );
                            }
                            if authority.snapshot_revision < established.authority.snapshot_revision
                            {
                                bail!(
                                    "coherent topology snapshot revision {} regressed behind \
                                     committed revision {}",
                                    authority.snapshot_revision.get(),
                                    established.authority.snapshot_revision.get()
                                );
                            }
                            (PreReadyUnilateralQueue::default(), established.events)
                        }
                    };
                    self.phase =
                        ClientTopologyPhase::AwaitingCommit(ClientTopologyAwaitingCommit {
                            authority,
                            legacy,
                            events,
                        });
                    metrics::counter!(
                        "mux.client.topology_fence.total",
                        "outcome" => "snapshot_delivered"
                    )
                    .increment(1);
                    Ok(ClientTopologyResponseAction::AwaitCommit)
                }
                ListPanesCoherentOutcome::Contended { .. } => {
                    let routed = self.restore_prior(in_flight.prior)?;
                    metrics::counter!(
                        "mux.client.topology_fence.total",
                        "outcome" => "contended"
                    )
                    .increment(1);
                    Ok(ClientTopologyResponseAction::Route(routed))
                }
                ListPanesCoherentOutcome::RevisionExhausted => {
                    Ok(ClientTopologyResponseAction::TerminalAfterDelivery(
                        "server topology revision authority is exhausted",
                    ))
                }
                ListPanesCoherentOutcome::Unsupported { .. } => {
                    Ok(ClientTopologyResponseAction::TerminalAfterDelivery(
                        "server rejected the required coherent topology fence",
                    ))
                }
            },
            Pdu::ErrorResponse(_) => {
                let routed = self.restore_prior(in_flight.prior)?;
                Ok(ClientTopologyResponseAction::Route(routed))
            }
            other => bail!(
                "unexpected {} response to coherent topology request",
                other.pdu_name()
            ),
        }
    }

    fn on_legacy_response(&mut self, serial: NonZeroU64) -> anyhow::Result<()> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::Fencing(in_flight) = phase else {
            self.phase = phase;
            bail!("legacy topology response arrived without an active local fence");
        };
        if in_flight.serial != serial {
            bail!(
                "legacy topology response serial {} did not match active fence {}",
                serial,
                in_flight.serial
            );
        }
        let ClientTopologyPrior::Legacy { buffered } = in_flight.prior else {
            bail!("legacy topology response crossed an established stamped stream");
        };
        self.phase =
            ClientTopologyPhase::LegacyAwaitingCommit(ClientLegacyTopologyAwaitingCommit {
                serial,
                buffered,
            });
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "legacy_snapshot_delivered"
        )
        .increment(1);
        Ok(())
    }

    fn on_legacy_rejection(&mut self, serial: NonZeroU64) -> anyhow::Result<Vec<DecodedPdu>> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::Fencing(in_flight) = phase else {
            self.phase = phase;
            bail!("legacy topology rejection arrived without an active local fence");
        };
        if in_flight.serial != serial {
            bail!(
                "legacy topology rejection serial {} did not match active fence {}",
                serial,
                in_flight.serial
            );
        }
        self.restore_prior(in_flight.prior)
    }

    fn commit_legacy(
        &mut self,
        authority: LegacyTopologyFenceAuthority,
    ) -> anyhow::Result<Vec<DecodedPdu>> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::LegacyAwaitingCommit(mut awaiting) = phase else {
            self.phase = phase;
            bail!("legacy topology commit arrived without a delivered snapshot");
        };
        if awaiting.serial != authority.serial {
            bail!("legacy topology commit authority did not match the delivered snapshot");
        }
        let mut routed = Vec::new();
        for queued in awaiting.buffered.take_all() {
            routed.push(queued.decode()?);
        }
        self.phase = ClientTopologyPhase::Legacy;
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "legacy_consumer_committed"
        )
        .increment(1);
        Ok(routed)
    }

    fn commit(&mut self, authority: TopologyFenceAuthority) -> anyhow::Result<Vec<DecodedPdu>> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::AwaitingCommit(mut awaiting) = phase else {
            self.phase = phase;
            bail!("topology snapshot commit arrived without a delivered snapshot");
        };
        if awaiting.authority != authority {
            bail!("topology snapshot commit authority did not match the delivered snapshot");
        }

        let discarded_legacy = awaiting.legacy.take_all().len();
        if discarded_legacy > 0 {
            metrics::counter!(
                "mux.client.topology_fence.events.total",
                "outcome" => "legacy_snapshot_subsumed"
            )
            .increment(discarded_legacy as u64);
        }

        let mut established = EstablishedClientTopologyStream {
            authority,
            next_revision: authority
                .snapshot_revision
                .get()
                .checked_add(1)
                .map(TopologyRevision::new),
            events: ClientTopologyEventBuffer::default(),
        };
        for retained in awaiting.events.take_all() {
            if retained.event.stream_id != authority.stream_id {
                bail!("buffered topology event changed stream identity before commit");
            }
            if retained.event.revision <= authority.snapshot_revision {
                metrics::counter!(
                    "mux.client.topology_fence.events.total",
                    "outcome" => "snapshot_subsumed"
                )
                .increment(1);
            } else {
                established.events.insert(retained.event)?;
            }
        }
        let routed = Self::drain_established(&mut established)?;
        self.phase = ClientTopologyPhase::Established(established);
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "consumer_committed"
        )
        .increment(1);
        Ok(routed)
    }

    fn reject(&mut self, authority: TopologyFenceAuthority) -> anyhow::Result<()> {
        let phase = std::mem::replace(&mut self.phase, ClientTopologyPhase::Closed);
        let ClientTopologyPhase::AwaitingCommit(awaiting) = phase else {
            self.phase = phase;
            bail!("topology snapshot rejection arrived without a delivered snapshot");
        };
        if awaiting.authority != authority {
            bail!("topology snapshot rejection authority did not match the delivered snapshot");
        }
        metrics::counter!(
            "mux.client.topology_fence.total",
            "outcome" => "consumer_rejected"
        )
        .increment(1);
        Ok(())
    }

    /// Reject a delivered snapshot and revoke its transport before publishing
    /// the terminal acknowledgement to the consumer.
    fn reject_and_retire_transport(
        &mut self,
        authority: TopologyFenceAuthority,
        dispatch_authority: &ClientDispatchAuthority,
    ) -> anyhow::Result<TopologySnapshotDecisionAck> {
        self.reject(authority)?;
        dispatch_authority.begin_rpc_transport_retirement()?;
        Ok(TopologySnapshotDecisionAck::RejectedTerminal)
    }

    fn restore_prior(&mut self, prior: ClientTopologyPrior) -> anyhow::Result<Vec<DecodedPdu>> {
        match prior {
            ClientTopologyPrior::Legacy { mut buffered } => {
                let mut routed = Vec::new();
                for queued in buffered.take_all() {
                    routed.push(queued.decode()?);
                }
                self.phase = ClientTopologyPhase::Legacy;
                Ok(routed)
            }
            ClientTopologyPrior::Established(mut established) => {
                let routed = Self::drain_established(&mut established)?;
                self.phase = ClientTopologyPhase::Established(established);
                Ok(routed)
            }
        }
    }

    fn retain_established_event(
        established: &mut EstablishedClientTopologyStream,
        event: TopologyEvent,
    ) -> anyhow::Result<()> {
        if event.stream_id != established.authority.stream_id {
            bail!("topology event carried the wrong established stream identity");
        }
        if event.revision <= established.authority.snapshot_revision {
            bail!(
                "topology event revision {} did not follow committed snapshot {}",
                event.revision.get(),
                established.authority.snapshot_revision.get()
            );
        }
        let Some(next_revision) = established.next_revision else {
            bail!("topology event arrived after revision namespace exhaustion");
        };
        if event.revision < next_revision {
            bail!(
                "stale or duplicate topology revision {} arrived after {}",
                event.revision.get(),
                next_revision.get()
            );
        }
        if event.revision > next_revision {
            metrics::counter!(
                "mux.client.topology_fence.events.total",
                "outcome" => "gap_buffered"
            )
            .increment(1);
        }
        established.events.insert(event)
    }

    fn drain_established(
        established: &mut EstablishedClientTopologyStream,
    ) -> anyhow::Result<Vec<DecodedPdu>> {
        let mut routed = Vec::new();
        let mut needs_resync = false;
        while let Some(next_revision) = established.next_revision {
            let Some(event) = established.events.remove(next_revision)? else {
                break;
            };
            needs_resync |= append_topology_event_to_legacy_unilaterals(event, &mut routed);
            metrics::counter!(
                "mux.client.topology_fence.events.total",
                "outcome" => "replayed"
            )
            .increment(1);
            established.next_revision = next_revision
                .get()
                .checked_add(1)
                .map(TopologyRevision::new);
        }
        if let Some(next_revision) = established.next_revision {
            if let Some((first_revision, _)) = established.events.events.first_key_value() {
                metrics::counter!(
                    "mux.client.topology_fence.events.total",
                    "outcome" => "gap_detected"
                )
                .increment(1);
                bail!(
                    "topology stream lost revision {} before retained revision {}",
                    next_revision.get(),
                    first_revision.get()
                );
            }
        }
        if needs_resync {
            // `TabResized` is the existing private client-side resync trigger;
            // its handler intentionally ignores the tab id and requests one
            // fresh coherent snapshot. Use a reserved synthetic id only inside
            // this process so structural stamped events are never silently
            // consumed without converging local topology.
            routed.push(DecodedPdu {
                pdu: Pdu::TabResized(codec::TabResized { tab_id: 0 }),
                serial: 0,
            });
            metrics::counter!(
                "mux.client.topology_fence.events.total",
                "outcome" => "coalesced_resync"
            )
            .increment(1);
        }
        Ok(routed)
    }
}

fn is_legacy_topology_pdu(pdu: &Pdu) -> bool {
    matches!(
        pdu,
        Pdu::PaneRemoved(_)
            | Pdu::WindowWorkspaceChanged(_)
            | Pdu::PaneFocused(_)
            | Pdu::TabResized(_)
            | Pdu::TabAddedToWindow(_)
            | Pdu::TabTitleChanged(_)
            | Pdu::WindowTitleChanged(_)
            | Pdu::RenameWorkspace(_)
    )
}

fn append_topology_event_to_legacy_unilaterals(
    event: TopologyEvent,
    routed: &mut Vec<DecodedPdu>,
) -> bool {
    let pdu = match event.event {
        TopologyEventKind::PaneAdded { .. }
        | TopologyEventKind::FloatingPaneSpawned { .. }
        | TopologyEventKind::WindowCreated { .. }
        | TopologyEventKind::WindowRemoved { .. }
        | TopologyEventKind::WindowInvalidated { .. } => return true,
        TopologyEventKind::Empty => return false,
        TopologyEventKind::PaneRemoved { pane_id } => {
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id })
        }
        TopologyEventKind::WindowWorkspaceChanged {
            window_id,
            workspace: Some(workspace),
        } => Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
            window_id,
            workspace,
        }),
        TopologyEventKind::WindowWorkspaceChanged {
            workspace: None, ..
        }
        | TopologyEventKind::TabAddedToWindow { .. }
        | TopologyEventKind::TabResized { .. } => return true,
        TopologyEventKind::PaneFocused { pane_id } => {
            Pdu::PaneFocused(codec::PaneFocused { pane_id })
        }
        TopologyEventKind::TabTitleChanged { tab_id, title } => {
            Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title })
        }
        TopologyEventKind::WindowTitleChanged { window_id, title } => {
            Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title })
        }
        TopologyEventKind::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => Pdu::RenameWorkspace(codec::RenameWorkspace {
            old_workspace,
            new_workspace,
        }),
    };
    routed.push(DecodedPdu { pdu, serial: 0 });
    false
}

#[derive(Default)]
struct RpcReadinessWaiters {
    waiting: Vec<Sender<anyhow::Result<()>>>,
}

impl RpcReadinessWaiters {
    fn admit(&mut self, promise: Sender<anyhow::Result<()>>) {
        let before = self.waiting.len();
        self.waiting.retain(|waiter| !waiter.is_closed());
        let cancelled = before - self.waiting.len();
        if cancelled != 0 {
            let cancelled = u64::try_from(cancelled)
                .expect("bounded mux RPC readiness-waiter count fits in u64");
            metrics::counter!(
                "mux.client.rpc.readiness_waiter.total",
                "outcome" => "cancelled"
            )
            .increment(cancelled);
        }
        if promise.is_closed() {
            metrics::counter!(
                "mux.client.rpc.readiness_waiter.total",
                "outcome" => "cancelled"
            )
            .increment(1);
            return;
        }
        if self.waiting.len() >= MAX_RPC_READINESS_WAITERS {
            let _ = promise.try_send(Err(anyhow!(
                "mux RPC readiness reached its {} coalesced-waiter limit",
                MAX_RPC_READINESS_WAITERS
            )));
            metrics::counter!(
                "mux.client.rpc.readiness_waiter.total",
                "outcome" => "limit_rejected"
            )
            .increment(1);
            return;
        }
        if let Err(error) = self.waiting.try_reserve(1) {
            let _ = promise.try_send(Err(anyhow!(
                "reserving a mux RPC readiness waiter failed: {error}"
            )));
            metrics::counter!(
                "mux.client.rpc.readiness_waiter.total",
                "outcome" => "reserve_failed"
            )
            .increment(1);
            return;
        }
        self.waiting.push(promise);
        metrics::counter!(
            "mux.client.rpc.readiness_waiter.total",
            "outcome" => "coalesced"
        )
        .increment(1);
        let depth = u32::try_from(self.waiting.len())
            .expect("bounded mux RPC readiness-waiter depth fits in u32");
        metrics::histogram!("mux.client.rpc.readiness_waiter.depth").record(f64::from(depth));
    }

    fn complete_success(&mut self) {
        for waiter in self.waiting.drain(..) {
            let outcome = match waiter.try_send(Ok(())) {
                Ok(()) => "delivered",
                Err(TrySendError::Closed(_)) => "cancelled",
                Err(TrySendError::Full(_)) => "full",
            };
            metrics::counter!(
                "mux.client.rpc.readiness_waiter_completion.total",
                "outcome" => outcome
            )
            .increment(1);
        }
    }

    fn complete_error(&mut self, message: &str) {
        for waiter in self.waiting.drain(..) {
            let outcome = match waiter.try_send(Err(anyhow!(message.to_string()))) {
                Ok(()) => "delivered_error",
                Err(TrySendError::Closed(_)) => "cancelled",
                Err(TrySendError::Full(_)) => "full",
            };
            metrics::counter!(
                "mux.client.rpc.readiness_waiter_completion.total",
                "outcome" => outcome
            )
            .increment(1);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RpcReadinessReplayAccounting {
    generation: NonZeroU64,
    replayed_pdus: usize,
    replayed_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RpcReadinessReplayCompletion {
    CurrentGeneration,
    RetiredGeneration,
}

enum RpcReadinessNextAction {
    AwaitInFlightReplay,
    CommitReady,
    StartReplay {
        batch: VecDeque<QuarantinedUnilateral>,
        replayed_bytes: usize,
    },
}

#[derive(Default)]
struct RpcReadinessCoordinator {
    waiters: RpcReadinessWaiters,
    replay: Option<RpcReadinessReplayAccounting>,
}

impl RpcReadinessCoordinator {
    fn admit(&mut self, promise: Sender<anyhow::Result<()>>) {
        self.waiters.admit(promise);
    }

    fn next_action(
        &mut self,
        generation: NonZeroU64,
        pre_ready_unilateral: &mut PreReadyUnilateralQueue,
    ) -> anyhow::Result<RpcReadinessNextAction> {
        if self.replay.is_some() {
            return Ok(RpcReadinessNextAction::AwaitInFlightReplay);
        }
        if pre_ready_unilateral.is_empty() {
            return Ok(RpcReadinessNextAction::CommitReady);
        }

        let (batch, replayed_bytes) = pre_ready_unilateral.take_batch()?;
        self.replay = Some(RpcReadinessReplayAccounting {
            generation,
            replayed_pdus: batch.len(),
            replayed_bytes,
        });
        Ok(RpcReadinessNextAction::StartReplay {
            batch,
            replayed_bytes,
        })
    }

    fn finish_replay(
        &mut self,
        reader_generation: NonZeroU64,
        replay_generation: NonZeroU64,
        replayed_pdus: usize,
        replayed_bytes: usize,
    ) -> anyhow::Result<RpcReadinessReplayCompletion> {
        // Replay workers are detached from the socket reader. A worker can
        // finish after its reader has retired and after the shared control
        // queue has been handed to a successor generation. That late
        // completion is not an obligation of the successor and, critically,
        // must not consume the successor's own in-flight replay accounting.
        if replay_generation < reader_generation {
            metrics::counter!(
                "mux.client.rpc.readiness_replay_completion.total",
                "outcome" => "retired_generation"
            )
            .increment(1);
            return Ok(RpcReadinessReplayCompletion::RetiredGeneration);
        }
        if replay_generation > reader_generation {
            bail!(
                "future pre-ready replay completion for mux RPC generation {} reached reader {}",
                replay_generation,
                reader_generation
            );
        }

        let Some(expected) = self.replay.take() else {
            bail!(
                "unexpected pre-ready replay completion for mux RPC generation {} on reader {}",
                replay_generation,
                reader_generation
            );
        };
        if replay_generation != expected.generation {
            bail!(
                "unexpected pre-ready replay completion for mux RPC generation {} on reader {}",
                replay_generation,
                reader_generation
            );
        }
        if replayed_pdus != expected.replayed_pdus || replayed_bytes != expected.replayed_bytes {
            bail!(
                "pre-ready replay accounting mismatch: expected {} PDUs/{} bytes, \
                 completed {} PDUs/{} bytes",
                expected.replayed_pdus,
                expected.replayed_bytes,
                replayed_pdus,
                replayed_bytes
            );
        }
        Ok(RpcReadinessReplayCompletion::CurrentGeneration)
    }

    fn replayed_in_flight(&self) -> (usize, usize) {
        self.replay.map_or((0, 0), |replay| {
            (replay.replayed_pdus, replay.replayed_bytes)
        })
    }

    fn complete_success(&mut self) {
        self.waiters.complete_success();
    }

    fn complete_error(&mut self, message: &str) {
        self.waiters.complete_error(message);
    }
}

fn spawn_pre_ready_unilateral_replay(
    dispatch_authority: ClientDispatchAuthority,
    generation: NonZeroU64,
    reader_sender: Sender<ReaderMessage>,
    batch: VecDeque<QuarantinedUnilateral>,
    replayed_bytes: usize,
) {
    let replayed_pdus = batch.len();
    let replay_scope = RpcGenerationScope::exact(
        reader_sender.clone(),
        Arc::clone(&dispatch_authority.rpc_transport),
        generation,
        true,
    );
    let mut abort_guard = replay_scope
        .fatal_abort_guard("pre-ready unilateral replay failed or was cancelled")
        .expect("an exact pre-ready replay scope always has a generation");
    let reservation = match reserve_client_main_thread(
        MainThreadServiceClass::Topology,
        replayed_bytes.saturating_add(4 * 1024),
        "pre-ready unilateral replay",
    ) {
        Ok(reservation) => reservation,
        Err(error) => {
            metrics::counter!(
                "mux.client.rpc.readiness_replay_completion.total",
                "outcome" => "scheduler_rejected"
            )
            .increment(1);
            log::error!(
                "failed to schedule mandatory pre-ready unilateral replay; exact RPC generation will be aborted: {error:#}"
            );
            return;
        }
    };
    reservation
        .spawn(async move {
            let result = async {
                for queued in batch {
                    let decoded = queued.decode()?;
                    process_unilateral_with_barrier(&dispatch_authority, decoded)
                        .await
                        .context("replaying a pre-ready unilateral PDU")?;
                }
                anyhow::Result::<()>::Ok(())
            }
            .await;

            let completion = ReaderMessage::FinishReadyReplay {
                generation,
                reader_sender: reader_sender.clone(),
                replayed_pdus,
                replayed_bytes,
                result,
            };
            match reader_sender.try_send(completion) {
                Ok(()) => abort_guard.disarm(),
                Err(_) => {
                    metrics::counter!(
                        "mux.client.rpc.readiness_replay_completion.total",
                        "outcome" => "reader_queue_unavailable"
                    )
                    .increment(1);
                }
            }
            anyhow::Result::<()>::Ok(())
        })
        .detach();
}

fn commit_rpc_transport_ready(
    dispatch_authority: &ClientDispatchAuthority,
    generation: NonZeroU64,
) -> anyhow::Result<()> {
    let lifecycle = dispatch_authority.rpc_transport.lifecycle.lock();
    if !matches!(
        lifecycle.phase,
        RpcTransportPhase::Live(observed) if observed == generation
    ) || dispatch_authority
        .rpc_transport
        .live_generation
        .load(AtomicOrdering::Acquire)
        != generation.get()
    {
        bail!(
            "mux RPC generation {} retired before readiness publication",
            generation
        );
    }
    if lifecycle.readiness_authority.generation != generation {
        bail!(
            "mux RPC readiness authority generation {} does not match reader {}",
            lifecycle.readiness_authority.generation,
            generation,
        );
    }
    lifecycle.readiness_authority.mark_ready()?;
    dispatch_authority
        .rpc_transport
        .ready_generation
        .store(generation.get(), AtomicOrdering::Release);
    Ok(())
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum NotReconnectableError {
    #[error("Client was destroyed")]
    ClientWasDestroyed,
    #[error("ordinary mux inbound protocol violation: {0}")]
    ProtocolViolation(#[source] OrdinaryMuxProtocolError),
    #[error(
        "inactive ordered-window PDU {ident} reached the ordinary mux client with serial \
         {serial}, encoded_payload_len={encoded_payload_len}, compressed={compressed}; \
         retire this transport without reconnecting"
    )]
    InactiveOrderedWindowPdu {
        ident: u64,
        serial: u64,
        encoded_payload_len: usize,
        compressed: bool,
    },
}

#[cold]
fn inactive_ordered_window_pdu_error(header: &PduFrameHeader) -> NotReconnectableError {
    metrics::counter!("mux.client.protocol.inactive_ordered_window_pdu.total").increment(1);
    NotReconnectableError::InactiveOrderedWindowPdu {
        ident: header.ident(),
        serial: header.serial(),
        encoded_payload_len: header.encoded_payload_len(),
        compressed: header.is_compressed(),
    }
}

fn classify_inbound_protocol_error(
    error: OrdinaryMuxProtocolError,
    header: &PduFrameHeader,
) -> NotReconnectableError {
    record_ordinary_mux_protocol_rejection(&error, "inbound", "header");
    let ordered_window = matches!(
        &error,
        OrdinaryMuxProtocolError::EndpointInactive { ident, .. }
            if Pdu::wire_spec_for_ident(*ident)
                .is_some_and(RpcProtocolAuthority::uses_ordered_window_capability)
    );
    if ordered_window {
        // Preserve the committed .12.1 no-reconnect diagnosis, but derive the
        // family from generated registry capability metadata. Admission itself
        // has already failed through the one exhaustive authority path above.
        inactive_ordered_window_pdu_error(header)
    } else {
        NotReconnectableError::ProtocolViolation(error)
    }
}

/// Apply ordinary-client header policy in the order required for fail-closed
/// feature gating: inactive families win before legacy serial correlation.
#[inline]
fn validate_ordinary_mux_inbound_header(
    rpc_transport: &RpcTransportState,
    generation: NonZeroU64,
    header: &PduFrameHeader,
    highest_issued: u64,
) -> anyhow::Result<()> {
    if let Err(error) = rpc_transport.validate_inbound_header(generation, header) {
        return Err(anyhow::Error::new(classify_inbound_protocol_error(
            error, header,
        )));
    }
    if header.serial() > highest_issued {
        return Err(anyhow::Error::new(CorruptResponse::SerialAboveCeiling {
            serial: header.serial(),
            max_serial: highest_issued,
        })
        .context("decoding a PDU"));
    }
    Ok(())
}

/// Post-materialization counterpart used only by the exact codec-46 decoder.
/// Current dialects retain the selector/header path above, including bounded
/// tombstone drainage.
#[inline]
fn validate_legacy_mux_inbound_identity(
    rpc_transport: &RpcTransportState,
    generation: NonZeroU64,
    serial: u64,
    ident: u64,
    highest_issued: u64,
) -> anyhow::Result<()> {
    if let Err(error) = rpc_transport.validate_inbound_identity(generation, serial, ident) {
        record_ordinary_mux_protocol_rejection(&error, "inbound", "legacy_materialized");
        return Err(anyhow::Error::new(
            NotReconnectableError::ProtocolViolation(error),
        ));
    }
    if serial > highest_issued {
        return Err(anyhow::Error::new(CorruptResponse::SerialAboveCeiling {
            serial,
            max_serial: highest_issued,
        })
        .context("decoding a legacy dialect PDU"));
    }
    Ok(())
}

#[derive(Debug)]
struct PendingRpc {
    completion: Sender<anyhow::Result<PendingRpcReply>>,
    binding: RpcBinding,
    stage: RpcRetirementStage,
    effect: PendingRpcEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRpcEffect {
    Ordinary,
    CoherentTopologyFence,
    LegacyTopologyFence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingResponseBodyDisposition {
    Materialize(PendingRpcEffect),
    DiscardKnownTombstone,
}

#[derive(Debug, Clone)]
struct RpcMetrics {
    pending: metrics::Gauge,
    admitted: metrics::Counter,
    preclosed: metrics::Counter,
    delivered: metrics::Counter,
    abandoned: metrics::Counter,
    transport_failed_live: metrics::Counter,
    transport_cleared_abandoned: metrics::Counter,
    retirement_reply_channel_full: metrics::Counter,
    future_serial: metrics::Counter,
    unmatched_serial: metrics::Counter,
    unexpected_response_ident: metrics::Counter,
    serial_exhausted: metrics::Counter,
    reserve_failed: metrics::Counter,
    serial_collision: metrics::Counter,
    protocol_reply_channel_full: metrics::Counter,
}

impl RpcMetrics {
    fn register() -> Self {
        Self {
            pending: metrics::gauge!("mux.client.rpc.pending"),
            admitted: metrics::counter!(
                "mux.client.rpc.admission.total",
                "outcome" => "admitted"
            ),
            preclosed: metrics::counter!(
                "mux.client.rpc.admission.total",
                "outcome" => "preclosed"
            ),
            delivered: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "delivered"
            ),
            abandoned: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "abandoned"
            ),
            transport_failed_live: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "transport_failed_live"
            ),
            transport_cleared_abandoned: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "transport_cleared_abandoned"
            ),
            retirement_reply_channel_full: metrics::counter!(
                "mux.client.rpc.retirement.total",
                "outcome" => "reply_channel_full"
            ),
            future_serial: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "future_serial"
            ),
            unmatched_serial: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "unmatched_serial"
            ),
            unexpected_response_ident: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "unexpected_response_ident"
            ),
            serial_exhausted: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "serial_exhausted"
            ),
            reserve_failed: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "reserve_failed"
            ),
            serial_collision: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "serial_collision"
            ),
            protocol_reply_channel_full: metrics::counter!(
                "mux.client.rpc.protocol_error.total",
                "kind" => "reply_channel_full"
            ),
        }
    }
}

#[derive(Error, Debug)]
enum PendingRpcError {
    #[error("mux client RPC serial space is exhausted")]
    SerialExhausted,
    #[error(transparent)]
    IncarnationTerminal(#[from] RpcTransportError),
    #[error("cannot reserve capacity for another pending mux client RPC")]
    Reserve(#[source] std::collections::TryReserveError),
    #[error(
        "mux client RPC serial {serial} for {request} collides with pending request {pending_request}"
    )]
    SerialCollision {
        serial: NonZeroU64,
        request: &'static str,
        pending_request: &'static str,
    },
    #[error(
        "server replied with future RPC serial {serial}; highest serial issued by this transport is {highest_issued}"
    )]
    FutureSerial {
        serial: NonZeroU64,
        highest_issued: u64,
    },
    #[error(
        "server replied with RPC serial {serial}, which is no longer pending (highest issued {highest_issued})"
    )]
    UnmatchedSerial {
        serial: NonZeroU64,
        highest_issued: u64,
    },
    #[error(
        "reply channel for RPC serial {serial} ({request} -> {response}) was unexpectedly full"
    )]
    ReplyChannelFull {
        serial: NonZeroU64,
        request: &'static str,
        response: &'static str,
    },
    #[error(
        "response serial {serial} belonged to generation {pending_generation}, \
         not active transport generation {transport_generation}"
    )]
    ResponseGenerationMismatch {
        serial: NonZeroU64,
        pending_generation: NonZeroU64,
        transport_generation: NonZeroU64,
    },
    #[error(
        "server replied to {request} RPC serial {serial} with unexpected PDU ident \
         {observed_ident}; expected ident {expected_response_ident} or ErrorResponse"
    )]
    UnexpectedResponseIdent {
        serial: NonZeroU64,
        request: &'static str,
        expected_response_ident: u64,
        observed_ident: u64,
    },
    #[error(
        "RPC serial {serial} lost its exact abandoned-body discard eligibility before retirement"
    )]
    DiscardEligibilityChanged { serial: NonZeroU64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyDisposition {
    Delivered,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplyCompletion {
    disposition: ReplyDisposition,
}

#[derive(Debug)]
struct PendingReplies {
    map: HashMap<NonZeroU64, PendingRpc>,
    highest_issued: u64,
    metrics: RpcMetrics,
    generation: NonZeroU64,
    rpc_transport: Arc<RpcTransportState>,
}

impl PendingReplies {
    fn new(
        metrics: RpcMetrics,
        generation: NonZeroU64,
        rpc_transport: Arc<RpcTransportState>,
    ) -> Self {
        Self {
            map: HashMap::new(),
            highest_issued: 0,
            metrics,
            generation,
            rpc_transport,
        }
    }

    /// Admit a request at the reader's local write-attempt boundary.
    ///
    /// If the caller closed its receiver while this request was still queued,
    /// it consumes neither a serial nor a wire frame. A close after this check
    /// is an admitted abandonment even if a later encode or flush fails, and
    /// retains this same map entry until reply drainage or transport teardown.
    fn admit(
        &mut self,
        completion: Sender<anyhow::Result<PendingRpcReply>>,
        binding: RpcBinding,
        effect: PendingRpcEffect,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        if completion.is_closed() {
            self.metrics.preclosed.increment(1);
            return Ok(None);
        }

        if binding.generation != self.generation {
            let error = self.rpc_transport.retirement_error(
                binding,
                RpcRetirementStage::SerialAssignment,
                RpcDeliveryCertainty::DefinitelyNotSent,
                "request generation does not match this transport actor",
            );
            let _ = completion.try_send(Err(anyhow::Error::new(error)));
            return Ok(None);
        }
        if let Err(error) = self.rpc_transport.validate(
            binding,
            RpcRetirementStage::SerialAssignment,
            RpcDeliveryCertainty::DefinitelyNotSent,
            "transport retired before serial assignment",
        ) {
            complete_with_rpc_transport_error(&completion, error);
            return Ok(None);
        }

        let serial = match self.rpc_transport.allocate_wire_serial() {
            Ok(serial) => serial,
            Err(PendingRpcError::SerialExhausted) => {
                self.metrics.serial_exhausted.increment(1);
                let error = self.rpc_transport.mark_incarnation_terminal(
                    RpcTransportError::WireSerialExhausted {
                        attempt_id: binding.attempt_id,
                        request: binding.request,
                    },
                );
                complete_with_rpc_transport_error(&completion, error.clone());
                return Err(PendingRpcError::IncarnationTerminal(error));
            }
            Err(error) => return Self::reject_admission(completion, error),
        };
        if let Err(err) = self.map.try_reserve(1) {
            self.metrics.reserve_failed.increment(1);
            return Self::reject_admission(completion, PendingRpcError::Reserve(err));
        };

        match self.map.entry(serial) {
            Entry::Vacant(entry) => {
                entry.insert(PendingRpc {
                    completion,
                    binding,
                    stage: RpcRetirementStage::SerialAssignment,
                    effect,
                });
            }
            Entry::Occupied(entry) => {
                self.metrics.serial_collision.increment(1);
                let error = PendingRpcError::SerialCollision {
                    serial,
                    request: binding.request,
                    pending_request: entry.get().binding.request,
                };
                return Self::reject_admission(completion, error);
            }
        }

        if let Err(error) = self.rpc_transport.validate(
            binding,
            RpcRetirementStage::SerialAssignment,
            RpcDeliveryCertainty::DefinitelyNotSent,
            "transport retired while assigning a serial",
        ) {
            let pending = self
                .map
                .remove(&serial)
                .expect("the just-inserted pending RPC must still exist");
            complete_with_rpc_transport_error(&pending.completion, error);
            return Ok(None);
        }

        self.highest_issued = serial.get();
        self.metrics.admitted.increment(1);
        // This gauge is process-wide and intentionally unlabeled. Treat it as
        // an aggregate across every concurrent mux connection; absolute
        // per-connection `set` operations would clobber one another.
        self.metrics.pending.increment(1);
        Ok(Some(serial))
    }

    fn set_stage(
        &mut self,
        serial: NonZeroU64,
        stage: RpcRetirementStage,
    ) -> Result<RpcBinding, PendingRpcError> {
        let Some(pending) = self.map.get_mut(&serial) else {
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        pending.stage = stage;
        Ok(pending.binding)
    }

    fn validate_stage(
        &self,
        serial: NonZeroU64,
        stage: RpcRetirementStage,
        certainty: RpcDeliveryCertainty,
        reason: &'static str,
    ) -> Result<(), RpcTransportError> {
        let pending = self
            .map
            .get(&serial)
            .expect("stage validation requires an admitted pending RPC");
        self.rpc_transport
            .validate(pending.binding, stage, certainty, reason)
    }

    fn highest_issued(&self) -> u64 {
        self.highest_issued
    }

    fn binding_for_correlated_response(&self, serial: NonZeroU64) -> RpcBinding {
        self.map
            .get(&serial)
            .expect("decoded correlated response must retain its pending RPC")
            .binding
    }

    fn response_body_disposition(
        &mut self,
        serial: NonZeroU64,
        header: &PduFrameHeader,
    ) -> Result<PendingResponseBodyDisposition, PendingRpcError> {
        let Some(pending) = self.map.get_mut(&serial) else {
            if serial.get() > self.highest_issued {
                self.metrics.future_serial.increment(1);
                return Err(PendingRpcError::FutureSerial {
                    serial,
                    highest_issued: self.highest_issued,
                });
            }
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        if pending.binding.generation != self.generation {
            return Err(PendingRpcError::ResponseGenerationMismatch {
                serial,
                pending_generation: pending.binding.generation,
                transport_generation: self.generation,
            });
        }
        // Header correlation is the ResponseMatch boundary even if the
        // generation retires immediately before validation. Record it first so
        // terminal diagnostics cannot regress to AwaitingResponse.
        pending.stage = RpcRetirementStage::ResponseMatch;
        self.rpc_transport.validate(
            pending.binding,
            RpcRetirementStage::ResponseMatch,
            RpcDeliveryCertainty::OutcomeUnknown,
            "transport retired before response body admission",
        )?;

        let observed_ident = header.ident();
        let error_response_ident = <ErrorResponse as PduWireIdent>::IDENT;
        if let Some(expected_response_ident) = pending.binding.expected_response_ident {
            if observed_ident != expected_response_ident.get()
                && observed_ident != error_response_ident
            {
                self.metrics.unexpected_response_ident.increment(1);
                return Err(PendingRpcError::UnexpectedResponseIdent {
                    serial,
                    request: pending.binding.request,
                    expected_response_ident: expected_response_ident.get(),
                    observed_ident,
                });
            }
        }
        let may_discard = pending.effect == PendingRpcEffect::Ordinary
            && pending.completion.is_closed()
            // Only the exact typed success response is eligible. In
            // particular, preserve ErrorResponse diagnostics and schema
            // validation even after the caller abandons its waiter.
            && pending.binding.expected_response_ident.map(NonZeroU64::get)
                == Some(observed_ident)
            && observed_ident != error_response_ident
            // Raw compressed-byte drainage would bypass the established
            // materialize/decompress/typed-schema path. Complete-frame,
            // decoder-window, and decompressed-size policy remains separate;
            // compressed tombstones conservatively stay on that path.
            && !header.is_compressed();

        if may_discard {
            Ok(PendingResponseBodyDisposition::DiscardKnownTombstone)
        } else {
            Ok(PendingResponseBodyDisposition::Materialize(pending.effect))
        }
    }

    /// Correlate an already materialized legacy frame.
    ///
    /// Codec-46 decoding intentionally does not use the current streaming
    /// selector because changed schemas must be classified by the exact
    /// dialect decoder. Consequently correlation happens here, after bounded
    /// decoding, and never enables the current tombstone fast path.
    fn legacy_materialized_response_effect(
        &mut self,
        serial: NonZeroU64,
        observed_ident: u64,
    ) -> Result<PendingRpcEffect, PendingRpcError> {
        let Some(pending) = self.map.get_mut(&serial) else {
            if serial.get() > self.highest_issued {
                self.metrics.future_serial.increment(1);
                return Err(PendingRpcError::FutureSerial {
                    serial,
                    highest_issued: self.highest_issued,
                });
            }
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        if pending.binding.generation != self.generation {
            return Err(PendingRpcError::ResponseGenerationMismatch {
                serial,
                pending_generation: pending.binding.generation,
                transport_generation: self.generation,
            });
        }
        pending.stage = RpcRetirementStage::ResponseMatch;
        self.rpc_transport.validate(
            pending.binding,
            RpcRetirementStage::ResponseMatch,
            RpcDeliveryCertainty::OutcomeUnknown,
            "transport retired before legacy response correlation",
        )?;

        let error_response_ident = <ErrorResponse as PduWireIdent>::IDENT;
        if let Some(expected_response_ident) = pending.binding.expected_response_ident {
            if observed_ident != expected_response_ident.get()
                && observed_ident != error_response_ident
            {
                self.metrics.unexpected_response_ident.increment(1);
                return Err(PendingRpcError::UnexpectedResponseIdent {
                    serial,
                    request: pending.binding.request,
                    expected_response_ident: expected_response_ident.get(),
                    observed_ident,
                });
            }
        }
        Ok(pending.effect)
    }

    fn complete_discarded_abandoned(
        &mut self,
        serial: NonZeroU64,
        observed_ident: u64,
    ) -> Result<(), PendingRpcError> {
        let Some(pending) = self.map.get(&serial) else {
            if serial.get() > self.highest_issued {
                self.metrics.future_serial.increment(1);
                return Err(PendingRpcError::FutureSerial {
                    serial,
                    highest_issued: self.highest_issued,
                });
            }
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        if pending.binding.generation != self.generation {
            return Err(PendingRpcError::ResponseGenerationMismatch {
                serial,
                pending_generation: pending.binding.generation,
                transport_generation: self.generation,
            });
        }
        self.rpc_transport.validate(
            pending.binding,
            RpcRetirementStage::ResponseMatch,
            RpcDeliveryCertainty::OutcomeUnknown,
            "transport retired while draining an abandoned response body",
        )?;
        let error_response_ident = <ErrorResponse as PduWireIdent>::IDENT;
        if pending.effect != PendingRpcEffect::Ordinary
            || !pending.completion.is_closed()
            || pending.binding.expected_response_ident.map(NonZeroU64::get) != Some(observed_ident)
            || observed_ident == error_response_ident
        {
            return Err(PendingRpcError::DiscardEligibilityChanged { serial });
        }

        let _pending = self
            .map
            .remove(&serial)
            .expect("validated abandoned RPC must remain present until exact body drainage");
        self.metrics.pending.decrement(1);
        self.metrics.abandoned.increment(1);
        Ok(())
    }

    fn complete(
        &mut self,
        serial: NonZeroU64,
        reply: PendingRpcReply,
    ) -> Result<ReplyCompletion, PendingRpcError> {
        let Some(pending) = self.map.remove(&serial) else {
            if serial.get() > self.highest_issued {
                self.metrics.future_serial.increment(1);
                return Err(PendingRpcError::FutureSerial {
                    serial,
                    highest_issued: self.highest_issued,
                });
            }
            self.metrics.unmatched_serial.increment(1);
            return Err(PendingRpcError::UnmatchedSerial {
                serial,
                highest_issued: self.highest_issued,
            });
        };
        self.metrics.pending.decrement(1);

        if pending.binding.generation != self.generation {
            let error = self.rpc_transport.make_retirement_error(
                pending.binding,
                RpcRetirementStage::ResponseMatch,
                RpcDeliveryCertainty::OutcomeUnknown,
                "response matched a pending request from another transport generation",
            );
            let disposition = complete_with_rpc_transport_error(&pending.completion, error);
            self.record_transport_error_completion(disposition);
            return Err(PendingRpcError::ResponseGenerationMismatch {
                serial,
                pending_generation: pending.binding.generation,
                transport_generation: self.generation,
            });
        }
        if let Err(error) = self.rpc_transport.validate(
            pending.binding,
            RpcRetirementStage::ResponseMatch,
            RpcDeliveryCertainty::OutcomeUnknown,
            "transport retired before response correlation completed",
        ) {
            let disposition = complete_with_rpc_transport_error(&pending.completion, error);
            self.record_transport_error_completion(disposition);
            return Err(PendingRpcError::ResponseGenerationMismatch {
                serial,
                pending_generation: pending.binding.generation,
                transport_generation: self.generation,
            });
        }

        let response_name = reply.response_name();
        match pending.completion.try_send(Ok(reply)) {
            Ok(()) => {
                // "delivered" is linearized at successful enqueue into the
                // one-shot channel; the caller may close before observing it.
                self.metrics.delivered.increment(1);
                Ok(ReplyCompletion {
                    disposition: ReplyDisposition::Delivered,
                })
            }
            Err(TrySendError::Closed(_)) => {
                self.metrics.abandoned.increment(1);
                Ok(ReplyCompletion {
                    disposition: ReplyDisposition::Abandoned,
                })
            }
            Err(TrySendError::Full(_)) => {
                self.metrics.retirement_reply_channel_full.increment(1);
                self.metrics.protocol_reply_channel_full.increment(1);
                Err(PendingRpcError::ReplyChannelFull {
                    serial,
                    request: pending.binding.request,
                    response: response_name,
                })
            }
        }
    }

    fn record_transport_error_completion(&self, disposition: RpcErrorCompletion) {
        match disposition {
            RpcErrorCompletion::Delivered => self.metrics.transport_failed_live.increment(1),
            RpcErrorCompletion::Abandoned => {
                self.metrics.transport_cleared_abandoned.increment(1);
            }
            RpcErrorCompletion::Full => {
                self.metrics.retirement_reply_channel_full.increment(1);
                self.metrics.protocol_reply_channel_full.increment(1);
            }
        }
    }

    #[cfg(test)]
    fn complete_or_fail_transport(
        &mut self,
        serial: NonZeroU64,
        pdu: Pdu,
    ) -> Result<ReplyCompletion, PendingRpcError> {
        match self.complete(serial, PendingRpcReply::pdu(pdu)) {
            Ok(disposition) => Ok(disposition),
            Err(error) => {
                self.fail_all(&error.to_string());
                Err(error)
            }
        }
    }

    fn fail_all(&mut self, reason: &str) {
        log::trace!("failing all pending RPCs: {reason}");
        for (serial, pending) in self.map.drain() {
            self.metrics.pending.decrement(1);
            if pending.completion.is_closed() {
                self.metrics.transport_cleared_abandoned.increment(1);
                continue;
            }
            let certainty = match pending.stage {
                RpcRetirementStage::Admission
                | RpcRetirementStage::Enqueue
                | RpcRetirementStage::Queued
                | RpcRetirementStage::Dequeue
                | RpcRetirementStage::SerialAssignment
                | RpcRetirementStage::FrameEncoding
                | RpcRetirementStage::BeforeWrite => RpcDeliveryCertainty::DefinitelyNotSent,
                RpcRetirementStage::WriteStarted
                | RpcRetirementStage::BeforeFlush
                | RpcRetirementStage::AfterFlush
                | RpcRetirementStage::AwaitingResponse
                | RpcRetirementStage::ResponseMatch
                | RpcRetirementStage::CompletionChannel
                | RpcRetirementStage::ConsumerCommit => RpcDeliveryCertainty::OutcomeUnknown,
            };
            let error = self.rpc_transport.retirement_error(
                pending.binding,
                pending.stage,
                certainty,
                format!("{reason} (pending serial {serial})"),
            );
            match pending.completion.try_send(Err(anyhow::Error::new(error))) {
                Ok(()) => self.metrics.transport_failed_live.increment(1),
                Err(TrySendError::Closed(_)) => {
                    self.metrics.transport_cleared_abandoned.increment(1);
                }
                Err(TrySendError::Full(_)) => {
                    self.metrics.retirement_reply_channel_full.increment(1);
                    self.metrics.protocol_reply_channel_full.increment(1);
                    log::error!(
                        "reply channel unexpectedly full while failing pending {} serial {}",
                        pending.binding.request,
                        serial
                    );
                }
            }
        }
    }

    fn record_decode_protocol_error(&self, error: &anyhow::Error) {
        if matches!(
            error.downcast_ref::<CorruptResponse>(),
            Some(CorruptResponse::SerialAboveCeiling { .. })
        ) {
            self.metrics.future_serial.increment(1);
        }
    }

    fn fail_after_transport_error(&mut self, error: &anyhow::Error) {
        self.fail_all(&format!("{error:#}"));
    }

    #[cfg(test)]
    fn fail_after_decode_error(&mut self, error: &anyhow::Error) {
        self.record_decode_protocol_error(error);
        self.fail_all(&format!("Error while decoding response pdu: {error:#}"));
    }

    fn reject_admission(
        completion: Sender<anyhow::Result<PendingRpcReply>>,
        error: PendingRpcError,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        let _ = completion.try_send(Err(anyhow!("{error}")));
        Err(error)
    }

    #[cfg(test)]
    fn admit_named(
        &mut self,
        completion: Sender<anyhow::Result<PendingRpcReply>>,
        request: &'static str,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        self.admit_named_expect(completion, request, None)
    }

    #[cfg(test)]
    fn admit_named_expect(
        &mut self,
        completion: Sender<anyhow::Result<PendingRpcReply>>,
        request: &'static str,
        expected_response_ident: Option<NonZeroU64>,
    ) -> Result<Option<NonZeroU64>, PendingRpcError> {
        let attempt_id = self
            .rpc_transport
            .allocate_attempt(request)
            .expect("test RPC attempt identity should be available");
        self.admit(
            completion,
            RpcBinding {
                generation: self.generation,
                attempt_id,
                request,
                expected_response_ident,
            },
            PendingRpcEffect::Ordinary,
        )
    }
}

impl Drop for PendingReplies {
    fn drop(&mut self) {
        self.fail_all("Client was destroyed");
    }
}

/// Process-wide Windows SSH fallback dispatcher.
///
/// Only the dispatcher owns an asupersync timer. Each read/write stream owns at
/// most one RAII reservation in the ordered queue; reset/drop unlinks it, while
/// cancellation of a direct poll can retain only that one bounded reservation
/// until it fires. Dispatch advances only after an actual target wake, which
/// preserves the 500-wake/second limit without a simultaneous boundary burst.
#[cfg(any(windows, test))]
struct BoundedPollCadence {
    state: ParkingMutex<BoundedPollCadenceState>,
    slot_spacing_micros: u64,
    now_micros: Arc<dyn Fn() -> u64 + Send + Sync>,
}

#[cfg(any(windows, test))]
struct BoundedPollCadenceState {
    next_reservation_id: u64,
    next_dispatch_micros: u64,
    reservations: BTreeMap<(u64, u64), BoundedPollEntry>,
    reservation_deadlines: BTreeMap<u64, u64>,
    active_timer: Option<BoundedPollTimer>,
}

#[cfg(any(windows, test))]
struct BoundedPollEntry {
    timer: TimerDriverHandle,
    waker: std::task::Waker,
    reservation_live: Arc<AtomicBool>,
}

#[cfg(any(windows, test))]
struct BoundedPollTimer {
    deadline_micros: u64,
    source_reservation_id: u64,
    timer: TimerDriverHandle,
    handle: TimerHandle,
    live: Arc<AtomicBool>,
}

#[cfg(any(windows, test))]
struct BoundedPollTimerWake {
    cadence: Weak<BoundedPollCadence>,
    live: Arc<AtomicBool>,
}

#[cfg(any(windows, test))]
impl futures::task::ArcWake for BoundedPollTimerWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if !arc_self.live.swap(false, AtomicOrdering::AcqRel) {
            return;
        }
        if let Some(cadence) = arc_self.cadence.upgrade() {
            cadence.timer_fired(&arc_self.live);
        }
    }
}

#[cfg(any(windows, test))]
struct BoundedPollReservation {
    cadence: Weak<BoundedPollCadence>,
    reservation_id: u64,
    reservation_live: Arc<AtomicBool>,
}

#[cfg(any(windows, test))]
impl Drop for BoundedPollCadence {
    fn drop(&mut self) {
        if let Some(active) = self.state.get_mut().active_timer.take() {
            active.live.store(false, AtomicOrdering::Release);
            let _ = active.timer.cancel(&active.handle);
        }
    }
}

#[cfg(any(windows, test))]
impl Drop for BoundedPollReservation {
    fn drop(&mut self) {
        if !self.reservation_live.swap(false, AtomicOrdering::AcqRel) {
            return;
        }
        if let Some(cadence) = self.cadence.upgrade() {
            cadence.cancel(self.reservation_id);
        }
    }
}

#[cfg(any(windows, test))]
impl BoundedPollCadence {
    fn with_now(
        slot_spacing_micros: u64,
        now_micros: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: ParkingMutex::new(BoundedPollCadenceState {
                next_reservation_id: 0,
                next_dispatch_micros: 0,
                reservations: BTreeMap::new(),
                reservation_deadlines: BTreeMap::new(),
                active_timer: None,
            }),
            slot_spacing_micros: slot_spacing_micros.max(1),
            now_micros: Arc::new(now_micros),
        })
    }

    fn reserve(
        self: &Arc<Self>,
        timer: TimerDriverHandle,
        waker: std::task::Waker,
        minimum_delay: Duration,
    ) -> BoundedPollReservation {
        let now_micros = (self.now_micros)();
        let minimum_delay_micros = u64::try_from(minimum_delay.as_micros()).unwrap_or(u64::MAX);
        let not_before_micros = now_micros.saturating_add(minimum_delay_micros);
        let mut state = self.state.lock();
        let reservation_id = Self::allocate_reservation_id(&mut state);
        let reservation_live = Arc::new(AtomicBool::new(true));
        let replaced_entry = state.reservations.insert(
            (not_before_micros, reservation_id),
            BoundedPollEntry {
                timer,
                waker,
                reservation_live: Arc::clone(&reservation_live),
            },
        );
        let replaced_deadline = state
            .reservation_deadlines
            .insert(reservation_id, not_before_micros);
        self.arm_next_timer(&mut state, now_micros);
        drop(state);
        debug_assert!(replaced_entry.is_none());
        debug_assert!(replaced_deadline.is_none());
        drop(replaced_entry);
        BoundedPollReservation {
            cadence: Arc::downgrade(self),
            reservation_id,
            reservation_live,
        }
    }

    fn allocate_reservation_id(state: &mut BoundedPollCadenceState) -> u64 {
        loop {
            let candidate = state.next_reservation_id;
            state.next_reservation_id = state.next_reservation_id.wrapping_add(1);
            if !state.reservation_deadlines.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn cancel(self: &Arc<Self>, reservation_id: u64) {
        let now_micros = (self.now_micros)();
        let removed_entry = {
            let mut state = self.state.lock();
            let Some(not_before_micros) = state.reservation_deadlines.remove(&reservation_id)
            else {
                return;
            };
            let removed_entry = state
                .reservations
                .remove(&(not_before_micros, reservation_id));
            self.arm_next_timer(&mut state, now_micros);
            removed_entry
        };
        debug_assert!(removed_entry.is_some());
        // A task waker is foreign state and may release the last task
        // reference. Never drop it while holding the cadence mutex.
        drop(removed_entry);
    }

    fn timer_fired(self: &Arc<Self>, fired_live: &Arc<AtomicBool>) {
        let now_micros = (self.now_micros)();
        let dispatch = {
            let mut state = self.state.lock();
            let current_deadline_micros = state
                .active_timer
                .as_ref()
                .filter(|active| Arc::ptr_eq(&active.live, fired_live))
                .map(|active| active.deadline_micros);
            let Some(current_deadline_micros) = current_deadline_micros else {
                return;
            };
            if current_deadline_micros > now_micros {
                // Wakers may legally be invoked spuriously. Retire this arm
                // and recreate it at the effective cadence deadline rather
                // than letting an early wake spend the next global slot.
                if let Some(active) = state.active_timer.as_ref() {
                    active.live.store(false, AtomicOrdering::Release);
                }
                self.arm_next_timer(&mut state, now_micros);
                return;
            }
            state.active_timer = None;

            let ready_key = state
                .reservations
                .first_key_value()
                .filter(|((not_before_micros, _), _)| *not_before_micros <= now_micros)
                .map(|(key, _)| *key);
            let Some((not_before_micros, reservation_id)) = ready_key else {
                self.arm_next_timer(&mut state, now_micros);
                return;
            };
            let entry = state
                .reservations
                .remove(&(not_before_micros, reservation_id))
                .expect("ready fallback-poll reservation must remain indexed");
            let removed_deadline = state.reservation_deadlines.remove(&reservation_id);
            debug_assert_eq!(removed_deadline, Some(not_before_micros));
            let should_wake = entry.reservation_live.swap(false, AtomicOrdering::AcqRel);
            if should_wake {
                state.next_dispatch_micros = state
                    .next_dispatch_micros
                    .max(now_micros)
                    .saturating_add(self.slot_spacing_micros);
            }
            self.arm_next_timer(&mut state, now_micros);
            Some((should_wake, entry.waker))
        };

        if let Some((should_wake, waker)) = dispatch {
            if should_wake {
                waker.wake();
            }
        }
    }

    fn arm_next_timer(self: &Arc<Self>, state: &mut BoundedPollCadenceState, now_micros: u64) {
        let next = state.reservations.first_key_value().map(
            |(&(not_before_micros, reservation_id), entry)| {
                let deadline_micros = not_before_micros
                    .max(state.next_dispatch_micros)
                    .max(now_micros);
                (deadline_micros, reservation_id, entry.timer.clone())
            },
        );

        if state.active_timer.as_ref().is_some_and(|active| {
            next.as_ref()
                .is_some_and(|(deadline_micros, reservation_id, _)| {
                    active.deadline_micros == *deadline_micros
                        && active.source_reservation_id == *reservation_id
                        && active.live.load(AtomicOrdering::Acquire)
                })
        }) {
            return;
        }

        if let Some(active) = state.active_timer.take() {
            active.live.store(false, AtomicOrdering::Release);
            let _ = active.timer.cancel(&active.handle);
        }

        let Some((deadline_micros, source_reservation_id, timer)) = next else {
            return;
        };
        let live = Arc::new(AtomicBool::new(true));
        let timer_waker = futures::task::waker(Arc::new(BoundedPollTimerWake {
            cadence: Arc::downgrade(self),
            live: Arc::clone(&live),
        }));
        let deadline =
            timer.now() + Duration::from_micros(deadline_micros.saturating_sub(now_micros));
        let handle = timer.register(deadline, timer_waker);
        state.active_timer = Some(BoundedPollTimer {
            deadline_micros,
            source_reservation_id,
            timer,
            handle,
            live,
        });
    }

    #[cfg(test)]
    fn pending_reservations(&self) -> usize {
        self.state.lock().reservations.len()
    }

    #[cfg(test)]
    fn active_timer_live_for_test(&self) -> Option<Arc<AtomicBool>> {
        self.state
            .lock()
            .active_timer
            .as_ref()
            .map(|active| Arc::clone(&active.live))
    }
}

#[cfg(any(windows, test))]
fn next_bounded_poll_backoff(backoff: &AtomicU8) -> Duration {
    let exponent = backoff
        .try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |current| {
            Some(current.saturating_add(1).min(6))
        })
        .unwrap_or(6)
        .min(6);
    Duration::from_millis((4_u64 << exponent).min(250))
}

struct SshPollFallback {
    backoff: AtomicU8,
    #[cfg(any(windows, test))]
    reservation: Mutex<SshPollFallbackReservationState>,
}

#[cfg(any(windows, test))]
struct SshPollFallbackReservationState {
    generation: Option<u64>,
    reservation: Option<BoundedPollReservation>,
}

impl SshPollFallback {
    fn new() -> Self {
        Self {
            backoff: AtomicU8::new(0),
            #[cfg(any(windows, test))]
            reservation: Mutex::new(SshPollFallbackReservationState {
                generation: Some(0),
                reservation: None,
            }),
        }
    }

    fn reset(&self) {
        self.backoff.store(0, AtomicOrdering::Release);
        #[cfg(any(windows, test))]
        {
            let retired = {
                let mut state = self.lock_reservation();
                state.generation = state.generation.and_then(|current| current.checked_add(1));
                state.reservation.take()
            };
            drop(retired);
        }
    }

    #[cfg(any(windows, test))]
    fn arm(
        &self,
        cadence: &Arc<BoundedPollCadence>,
        timer: TimerDriverHandle,
        waker: std::task::Waker,
        minimum_delay: Duration,
    ) -> bool {
        let (generation, retired) = {
            let mut state = self.lock_reservation();
            let generation = state.generation.and_then(|current| current.checked_add(1));
            state.generation = generation;
            (generation, state.reservation.take())
        };
        // Cancelling can release a foreign task waker, so never do it while
        // holding the per-stream mutex.
        drop(retired);
        let Some(generation) = generation else {
            // Generation exhaustion is terminal. Never wrap to an authority
            // value that an arbitrarily old in-flight arm may still hold.
            return false;
        };
        let mut state = self.lock_reservation();
        if state.generation != Some(generation) {
            return false;
        }
        // Keep publication atomic with respect to reset: once the queue sees
        // this wake, the exact stream generation already owns its RAII handle.
        // The cadence never locks a stream, so stream -> cadence is acyclic.
        debug_assert!(state.reservation.is_none());
        state.reservation = Some(cadence.reserve(timer, waker, minimum_delay));
        true
    }

    #[cfg(any(windows, test))]
    fn lock_reservation(&self) -> MutexGuard<'_, SshPollFallbackReservationState> {
        match self.reservation.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.reservation.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

struct SshPollResetOnDrop<'a>(&'a SshPollFallback);

impl Drop for SshPollResetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.reset();
    }
}

#[cfg(windows)]
static WINDOWS_SSH_POLL_EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);
#[cfg(windows)]
static WINDOWS_SSH_POLL_CADENCE: std::sync::LazyLock<Arc<BoundedPollCadence>> =
    std::sync::LazyLock::new(|| {
        BoundedPollCadence::with_now(2_000, || {
            u64::try_from(WINDOWS_SSH_POLL_EPOCH.elapsed().as_micros()).unwrap_or(u64::MAX)
        })
    });

#[cfg(windows)]
fn fallback_rewake(current: &Cx, task_cx: &TaskContext<'_>, fallback: &SshPollFallback) -> bool {
    if let Some(timer) = current.timer_driver() {
        let minimum_delay = next_bounded_poll_backoff(&fallback.backoff);
        fallback.arm(
            &WINDOWS_SSH_POLL_CADENCE,
            timer,
            task_cx.waker().clone(),
            minimum_delay,
        )
    } else {
        false
    }
}

fn route_client_unilateral_batch(
    dispatch_authority: &ClientDispatchAuthority,
    generation: NonZeroU64,
    readiness: &RpcReadinessCoordinator,
    pre_ready_unilateral: &mut PreReadyUnilateralQueue,
    decoded_pdus: Vec<DecodedPdu>,
) -> anyhow::Result<()> {
    for decoded in decoded_pdus {
        let generation_is_ready = dispatch_authority
            .rpc_transport
            .ready_generation
            .load(AtomicOrdering::Acquire)
            == generation.get();
        if generation_is_ready {
            process_unilateral(dispatch_authority, decoded)
                .context("processing unilateral PDU from server")?;
        } else {
            let (replayed_pdus, replayed_bytes) = readiness.replayed_in_flight();
            pre_ready_unilateral
                .enqueue(decoded, replayed_pdus, replayed_bytes)
                .context("quarantining a unilateral PDU before mux RPC readiness")?;
        }
    }
    Ok(())
}

type ClientThreadOutcome = (anyhow::Result<()>, Reconnectable, Receiver<ReaderMessage>);

fn client_thread(
    mut reconnectable: Reconnectable,
    mut rx: Receiver<ReaderMessage>,
    dispatch_authority: ClientDispatchAuthority,
) -> ClientThreadOutcome {
    // The reader performs ALL of this connection's socket I/O, so it must run as
    // a scheduler-managed task (block_on_io) rather than a directly-polled
    // block_on future. asupersync only delivers socket-readiness wakeups to
    // tasks living on the scheduler; a future polled directly by block_on just
    // parks the thread and never gets the wakeup, so the handshake reply that
    // arrives *after* the reader parks (any real, latency-bearing connection
    // such as an SSH-proxy mux domain) is never consumed and the version check
    // times out. We move the owned reconnectable + receiver into the task and
    // return them so the reconnect loop can reuse them on the next attempt.
    // Erase the deeply nested reader future before `block_on_io` wraps it in
    // the asupersync spawn machinery. This is a compile-time normalization
    // boundary only: it prevents rustc #159228's `CoerceUnsized` obligation
    // from recursively expanding the full reader state machine, while the
    // exact owned future, output, scheduler, and blocking semantics remain.
    let reader_task: Pin<Box<dyn Future<Output = ClientThreadOutcome> + Send + 'static>> =
        Box::pin(async move {
            let result =
                client_thread_async(&mut reconnectable, &mut rx, &dispatch_authority).await;
            (result, reconnectable, rx)
        });
    promise::spawn::block_on_io(reader_task)
}

async fn client_thread_async(
    reconnectable: &mut Reconnectable,
    rx: &mut Receiver<ReaderMessage>,
    dispatch_authority: &ClientDispatchAuthority,
) -> anyhow::Result<()> {
    let generation = NonZeroU64::new(dispatch_authority.generation)
        .ok_or_else(|| anyhow!("mux client reader cannot own generation zero"))?;
    let reader_abort = dispatch_authority
        .rpc_transport
        .reader_abort_for_reader(generation)?;
    let mut pending = PendingReplies::new(
        RpcMetrics::register(),
        generation,
        Arc::clone(&dispatch_authority.rpc_transport),
    );

    // Wrap the connection in a persistent buffered reader so PDU decoding pulls
    // its leb128 length headers and bodies from an in-memory buffer (one socket
    // read per refill, default 8 KiB) instead of one syscall per byte
    // (decode_async reads leb128 a byte at a time — ~30 syscalls per PDU). The
    // BufReader lives across the whole reader loop so partially-buffered and
    // pipelined PDUs carry over between iterations. Writes still go straight to
    // the socket via `get_mut()` (BufReader only buffers reads), so the encode
    // path is byte-for-byte unchanged.
    let stream = match reconnectable.take_stream() {
        Some(stream) => stream,
        None => {
            let error =
                anyhow::anyhow!("mux client stream not available — connection not established");
            if let Err(retirement_error) = dispatch_authority.begin_rpc_transport_retirement() {
                log::error!(
                    "failed to retire mux client RPC transport without an installed stream: \
                     {retirement_error:#}"
                );
            }
            pending.fail_after_transport_error(&error);
            return Err(error);
        }
    };
    let mut reader = BufReader::new(stream);
    let mut pre_ready_unilateral = PreReadyUnilateralQueue::default();
    let mut readiness = RpcReadinessCoordinator::default();
    let mut topology = ClientTopologyCoordinator::default();

    enum NextEvent {
        Message(Result<ReaderMessage, async_channel::RecvError>),
        Readable(anyhow::Result<()>),
    }

    // The decoded branch deliberately keeps the already-inline `Pdu` on the
    // stack. Boxing it here would add one heap allocation to every live reply
    // and unilateral frame just to shrink this loop-local branch carrier.
    #[allow(clippy::large_enum_variant)]
    enum InboundPdu {
        Decoded {
            decoded: DecodedPdu,
            effect: Option<PendingRpcEffect>,
        },
        DecodedLegacy {
            decoded: DecodedMuxWirePdu,
            effect: Option<PendingRpcEffect>,
        },
        Discarded {
            serial: NonZeroU64,
            body: DiscardedPduBody,
        },
    }

    let result = async {
        loop {
            let next_event = {
                let rx_msg = rx.recv();
                // Readiness must be buffer-aware: a prior socket read may have
                // already buffered a complete (pipelined) PDU while the underlying
                // socket has nothing pending. Waiting on the socket alone would then
                // strand that buffered PDU until more bytes happen to arrive (a
                // latency stall / hang). So treat a non-empty buffer as immediately
                // readable and only park on the socket once the buffer is drained.
                //
                // Construct this future outside an `async` block. Capturing
                // `&reader` in an outer block across `.await` would require the
                // erased stream to be `Sync`, while the reader task only requires
                // its owned stream to be `Send`. This inner scope also drops the
                // losing select future before the match below mutably borrows the
                // buffered reader for write, flush, or decode.
                let wait_for_read = if reader.buffer().is_empty() {
                    Either::Left(reader.get_ref().wait_for_readable())
                } else {
                    Either::Right(ready(Ok::<(), anyhow::Error>(())))
                };
                pin_mut!(rx_msg);
                pin_mut!(wait_for_read);

                let selected = dispatch_authority
                    .rpc_transport
                    .complete_before_reader_stop(&reader_abort, select(rx_msg, wait_for_read))
                    .await?;
                match selected {
                    Either::Left((message, _)) => NextEvent::Message(message),
                    Either::Right((readable, _)) => NextEvent::Readable(readable),
                }
            };

            match next_event {
                NextEvent::Message(Ok(ReaderMessage::CommitTopologySnapshot {
                    generation: committed_generation,
                    authority,
                    promise,
                })) => {
                    if committed_generation != generation {
                        let _ = promise.try_send(Err(anyhow!(
                            "topology snapshot commit for generation {} reached reader {}",
                            committed_generation,
                            generation
                        )));
                        continue;
                    }
                    let routed = match topology.commit(authority) {
                        Ok(routed) => routed,
                        Err(error) => {
                            let message = format!(
                                "topology snapshot commit failed on generation {}: {error:#}",
                                generation
                            );
                            let _ = promise.try_send(Err(anyhow!(message.clone())));
                            return Err(error).context(message);
                        }
                    };
                    if let Err(error) = dispatch_authority
                        .rpc_transport
                        .bind_render_connection_identity(
                            generation,
                            authority.render_connection_identity(),
                        )
                    {
                        let message = format!(
                            "render connection identity bind failed on generation {}: {error:#}",
                            generation
                        );
                        let _ = promise.try_send(Err(anyhow!(message.clone())));
                        return Err(error).context(message);
                    }
                    if let Err(error) = route_client_unilateral_batch(
                        dispatch_authority,
                        generation,
                        &readiness,
                        &mut pre_ready_unilateral,
                        routed,
                    ) {
                        let message = format!(
                            "topology snapshot commit replay failed on generation {}: {error:#}",
                            generation
                        );
                        let _ = promise.try_send(Err(anyhow!(message.clone())));
                        return Err(error).context(message);
                    }
                    match promise.try_send(Ok(TopologySnapshotDecisionAck::CommittedLive)) {
                        Ok(()) | Err(TrySendError::Closed(_)) => {}
                        Err(TrySendError::Full(_)) => {
                            bail!(
                                "topology snapshot commit acknowledgement channel was full"
                            );
                        }
                    }
                }
                NextEvent::Message(Ok(ReaderMessage::RejectTopologySnapshot {
                    generation: rejected_generation,
                    authority,
                    promise,
                })) => {
                    if rejected_generation != generation {
                        let _ = promise.try_send(Err(anyhow!(
                            "topology snapshot rejection for generation {} reached reader {}",
                            rejected_generation,
                            generation
                        )));
                        continue;
                    }
                    let acknowledgement = match topology
                        .reject_and_retire_transport(authority, dispatch_authority)
                    {
                        Ok(acknowledgement) => acknowledgement,
                        Err(error) => {
                            let message = format!(
                                "topology snapshot rejection or retirement failed on generation \
                                 {}: {error:#}",
                                generation
                            );
                            let _ = promise.try_send(Err(anyhow!(message.clone())));
                            return Err(error).context(message);
                        }
                    };
                    match promise.try_send(Ok(acknowledgement)) {
                        Ok(()) | Err(TrySendError::Closed(_)) => {}
                        Err(TrySendError::Full(_)) => {
                            bail!(
                                "topology snapshot rejection acknowledgement channel was full"
                            );
                        }
                    }
                    bail!(
                        "coherent topology snapshot consumer rejected generation {}",
                        generation
                    );
                }
                NextEvent::Message(Ok(ReaderMessage::CommitLegacyTopologySnapshot {
                    generation: committed_generation,
                    authority,
                    promise,
                })) => {
                    if committed_generation != generation || authority.generation != generation {
                        let _ = promise.try_send(Err(anyhow!(
                            "legacy topology commit for generation {} reached reader {}",
                            committed_generation,
                            generation
                        )));
                        continue;
                    }
                    let routed = match topology.commit_legacy(authority) {
                        Ok(routed) => routed,
                        Err(error) => {
                            let message = format!(
                                "legacy topology commit failed on generation {}: {error:#}",
                                generation
                            );
                            let _ = promise.try_send(Err(anyhow!(message.clone())));
                            return Err(error).context(message);
                        }
                    };
                    if let Err(error) = route_client_unilateral_batch(
                        dispatch_authority,
                        generation,
                        &readiness,
                        &mut pre_ready_unilateral,
                        routed,
                    ) {
                        let message = format!(
                            "legacy topology replay failed on generation {}: {error:#}",
                            generation
                        );
                        let _ = promise.try_send(Err(anyhow!(message.clone())));
                        return Err(error).context(message);
                    }
                    match promise.try_send(Ok(())) {
                        Ok(()) | Err(TrySendError::Closed(_)) => {}
                        Err(TrySendError::Full(_)) => {
                            bail!("legacy topology commit acknowledgement channel was full");
                        }
                    }
                }
                NextEvent::Message(Ok(ReaderMessage::PublishReady {
                    generation: published_generation,
                    reader_sender,
                    promise,
                    reservation: _reservation,
                })) => {
                    if published_generation != generation {
                        let _ = promise.try_send(Err(anyhow!(
                            "readiness publication for generation {} reached reader {}",
                            published_generation,
                            generation
                        )));
                        continue;
                    }
                    if dispatch_authority
                        .rpc_transport
                        .ready_generation
                        .load(AtomicOrdering::Acquire)
                        == generation.get()
                    {
                        let outcome = match promise.try_send(Ok(())) {
                            Ok(()) => "delivered",
                            Err(TrySendError::Closed(_)) => "cancelled",
                            Err(TrySendError::Full(_)) => "full",
                        };
                        metrics::counter!(
                            "mux.client.rpc.readiness_waiter_completion.total",
                            "outcome" => outcome
                        )
                        .increment(1);
                        continue;
                    }
                    readiness.admit(promise);
                    match readiness.next_action(generation, &mut pre_ready_unilateral)? {
                        RpcReadinessNextAction::AwaitInFlightReplay => {}
                        RpcReadinessNextAction::CommitReady => {
                            if let Err(error) =
                                commit_rpc_transport_ready(dispatch_authority, generation)
                            {
                                readiness.complete_error(&format!("{error:#}"));
                                return Err(error);
                            }
                            readiness.complete_success();
                        }
                        RpcReadinessNextAction::StartReplay {
                            batch,
                            replayed_bytes,
                        } => {
                            spawn_pre_ready_unilateral_replay(
                                dispatch_authority.clone(),
                                generation,
                                reader_sender,
                                batch,
                                replayed_bytes,
                            );
                        }
                    }
                }
                NextEvent::Message(Ok(ReaderMessage::FinishReadyReplay {
                    generation: replay_generation,
                    reader_sender,
                    replayed_pdus,
                    replayed_bytes,
                    result,
                })) => {
                    let replay_completion = match readiness.finish_replay(
                        generation,
                        replay_generation,
                        replayed_pdus,
                        replayed_bytes,
                    ) {
                        Ok(completion) => completion,
                        Err(error) => {
                            readiness.complete_error(&format!("{error:#}"));
                            return Err(error);
                        }
                    };
                    if replay_completion == RpcReadinessReplayCompletion::RetiredGeneration {
                        log::trace!(
                            "discarding readiness replay completion for retired mux RPC \
                             generation {} on reader {}",
                            replay_generation,
                            generation
                        );
                        continue;
                    }
                    match result {
                        Ok(()) => {}
                        Err(error) => {
                            let message = format!(
                                "pre-ready unilateral replay failed for mux RPC generation \
                                 {}: {error:#}",
                                generation
                            );
                            readiness.complete_error(&message);
                            return Err(error).context(message);
                        }
                    }

                    match readiness.next_action(generation, &mut pre_ready_unilateral)? {
                        RpcReadinessNextAction::AwaitInFlightReplay => {
                            unreachable!(
                                "a completed replay cannot leave the same replay in flight"
                            );
                        }
                        RpcReadinessNextAction::CommitReady => {
                            if let Err(error) =
                                commit_rpc_transport_ready(dispatch_authority, generation)
                            {
                                readiness.complete_error(&format!("{error:#}"));
                                return Err(error);
                            }
                            readiness.complete_success();
                        }
                        RpcReadinessNextAction::StartReplay {
                            batch,
                            replayed_bytes,
                        } => {
                            spawn_pre_ready_unilateral_replay(
                                dispatch_authority.clone(),
                                generation,
                                reader_sender,
                                batch,
                                replayed_bytes,
                            );
                        }
                    }
                }
                NextEvent::Message(Ok(ReaderMessage::SendPdu {
                    binding,
                    lease,
                    promise,
                })) => {
                    if !lease.matches(binding) {
                        return Err(anyhow!(
                            "mux client outbound lease does not match its exact binding"
                        ));
                    }
                    if binding.generation != generation {
                        let error = dispatch_authority.rpc_transport.make_retirement_error(
                            binding,
                            RpcRetirementStage::Dequeue,
                            RpcDeliveryCertainty::DefinitelyNotSent,
                            "request reached a reader other than its bound transport",
                        );
                        complete_with_rpc_transport_error(&promise, error);
                        continue;
                    }
                    if let Err(error) = dispatch_authority.rpc_transport.validate(
                        binding,
                        RpcRetirementStage::Dequeue,
                        RpcDeliveryCertainty::DefinitelyNotSent,
                        "request transport retired before reader dequeue",
                    ) {
                        complete_with_rpc_transport_error(&promise, error);
                        continue;
                    }
                    let Some(prepared) = lease.claim_for_reader()? else {
                        if !promise.is_closed() {
                            bail!(
                                "mux client outbound lease settled before reader claim while its \
                                 completion channel remained open"
                            );
                        }
                        continue;
                    };
                    if let Err(error) = dispatch_authority
                        .rpc_transport
                        .validate_dequeued_outbound(generation, prepared.pdu())
                    {
                        record_ordinary_mux_protocol_rejection(
                            &error,
                            "outbound",
                            "dequeue",
                        );
                        match promise.try_send(Err(anyhow::Error::new(error))) {
                            Ok(()) | Err(TrySendError::Closed(_)) => continue,
                            Err(TrySendError::Full(_)) => {
                                bail!(
                                    "ordinary mux outbound protocol rejection found a full \
                                     completion channel"
                                );
                            }
                        }
                    }
                    let effect = if matches!(prepared.pdu(), Pdu::ListPanesCoherent(_)) {
                        PendingRpcEffect::CoherentTopologyFence
                    } else if prepared.dialect().is_legacy46()
                        && matches!(prepared.pdu(), Pdu::ListPanes(_))
                    {
                        PendingRpcEffect::LegacyTopologyFence
                    } else {
                        PendingRpcEffect::Ordinary
                    };
                    let serial = match pending.admit(promise, binding, effect) {
                        Ok(Some(serial)) => serial,
                        Ok(None) => {
                            dispatch_authority
                                .rpc_transport
                                .rollback_unadmitted_outbound(generation, prepared.pdu())
                                .map_err(anyhow::Error::new)?;
                            continue;
                        }
                        Err(PendingRpcError::IncarnationTerminal(error)) => {
                            return Err(anyhow::Error::new(error));
                        }
                        Err(error) => return Err(anyhow::Error::new(error)),
                    };
                    match effect {
                        PendingRpcEffect::CoherentTopologyFence => {
                            topology
                                .begin_fence(serial)
                                .context("admitting a coherent client topology fence")?;
                        }
                        PendingRpcEffect::LegacyTopologyFence => {
                            topology
                                .begin_legacy_fence(serial)
                                .context("admitting a local codec-46 topology fence")?;
                        }
                        PendingRpcEffect::Ordinary => {}
                    }

                    pending.set_stage(serial, RpcRetirementStage::FrameEncoding)?;
                    if let Err(error) = pending.validate_stage(
                        serial,
                        RpcRetirementStage::FrameEncoding,
                        RpcDeliveryCertainty::DefinitelyNotSent,
                        "transport retired before frame encoding",
                    ) {
                        return Err(anyhow::Error::new(error));
                    }
                    // Build the complete frame before touching the socket. Besides
                    // eliminating ambiguity at the encode boundary, this lets a
                    // retired request prove zero wire bytes before `write_all`.
                    let frame = match (*prepared)
                        .encode_frame(serial.get())
                        .context("encoding a PDU frame to send to the server")
                    {
                        Ok(frame) => frame,
                        Err(error) => {
                            return Err(error);
                        }
                    };
                    pending.set_stage(serial, RpcRetirementStage::BeforeWrite)?;
                    if let Err(error) = pending.validate_stage(
                        serial,
                        RpcRetirementStage::BeforeWrite,
                        RpcDeliveryCertainty::DefinitelyNotSent,
                        "transport retired after frame encoding and before socket write",
                    ) {
                        return Err(anyhow::Error::new(error));
                    }

                    pending.set_stage(serial, RpcRetirementStage::WriteStarted)?;
                    dispatch_authority
                        .rpc_transport
                        .complete_before_reader_stop(
                            &reader_abort,
                            reader.get_mut().write_all(&frame),
                        )
                        .await?
                        .context("writing an encoded PDU frame to the server")?;

                    pending.set_stage(serial, RpcRetirementStage::BeforeFlush)?;
                    if let Err(error) = pending.validate_stage(
                        serial,
                        RpcRetirementStage::BeforeFlush,
                        RpcDeliveryCertainty::OutcomeUnknown,
                        "transport retired after socket write and before flush",
                    ) {
                        return Err(anyhow::Error::new(error));
                    }
                    dispatch_authority
                        .rpc_transport
                        .complete_before_reader_stop(&reader_abort, reader.get_mut().flush())
                        .await?
                        .context("flushing PDU to server")?;
                    pending.set_stage(serial, RpcRetirementStage::AfterFlush)?;
                    if let Err(error) = pending.validate_stage(
                        serial,
                        RpcRetirementStage::AfterFlush,
                        RpcDeliveryCertainty::OutcomeUnknown,
                        "transport retired while flushing a request",
                    ) {
                        return Err(anyhow::Error::new(error));
                    }
                    pending.set_stage(serial, RpcRetirementStage::AwaitingResponse)?;
                    // The socket accepted and flushed the complete frame. No
                    // codec-owned outbound bytes remain live after these two
                    // owners drop; the pending reply retains only correlation
                    // state and the caller's completion channel.
                    drop(frame);
                    drop(lease);
                }
                NextEvent::Message(Err(_)) => {
                    return Err(NotReconnectableError::ClientWasDestroyed.into());
                }
                NextEvent::Readable(Ok(())) => {
                    let rpc_transport = Arc::clone(&dispatch_authority.rpc_transport);
                    let inbound = rpc_transport
                        .complete_before_reader_stop(&reader_abort, async {
                            let highest_issued = pending.highest_issued();
                            let dialect = rpc_transport
                                .wire_dialect(generation)
                                .map_err(NotReconnectableError::ProtocolViolation)?;
                            if dialect.is_legacy46() {
                                let decoded = Pdu::decode_async_for_dialect(
                                    &mut reader,
                                    None,
                                    dialect,
                                )
                                .await?;
                                let serial = decoded.serial();
                                let ident = decoded.payload().ident();
                                validate_legacy_mux_inbound_identity(
                                    &rpc_transport,
                                    generation,
                                    serial,
                                    ident,
                                    highest_issued,
                                )?;
                                let effect = NonZeroU64::new(serial)
                                    .map(|serial| {
                                        pending.legacy_materialized_response_effect(serial, ident)
                                    })
                                    .transpose()?;
                                return Ok(InboundPdu::DecodedLegacy { decoded, effect });
                            }

                            let mut selected_effect = None;
                            // The codec's optional serial ceiling is checked
                            // before it reads the PDU identity. Pass `None` so
                            // the selector can classify every inactive or
                            // misdirected identity for every serial, then
                            // reapply the same legacy ceiling in
                            // `validate_ordinary_mux_inbound_header`.
                            let decoded = Pdu::decode_async_with_selector(
                                &mut reader,
                                None,
                                |header| {
                                    validate_ordinary_mux_inbound_header(
                                        &rpc_transport,
                                        generation,
                                        header,
                                        highest_issued,
                                    )?;
                                    let Some(serial) = NonZeroU64::new(header.serial()) else {
                                        return Ok(PduBodyDisposition::Materialize);
                                    };
                                    match pending.response_body_disposition(serial, header)? {
                                        PendingResponseBodyDisposition::Materialize(effect) => {
                                            selected_effect = Some(effect);
                                            Ok(PduBodyDisposition::Materialize)
                                        }
                                        PendingResponseBodyDisposition::DiscardKnownTombstone => {
                                            Ok(PduBodyDisposition::Discard)
                                        }
                                    }
                                },
                            )
                            .await?;

                            match decoded {
                                AsyncPduDecode::Decoded(decoded) => Ok(InboundPdu::Decoded {
                                    decoded,
                                    effect: selected_effect,
                                }),
                                AsyncPduDecode::Discarded {
                                    serial,
                                    ident,
                                    body,
                                } => {
                                    let serial = NonZeroU64::new(serial).ok_or_else(|| {
                                        anyhow!(
                                            "codec discarded a serial-zero unilateral PDU body"
                                        )
                                    })?;
                                    pending.complete_discarded_abandoned(serial, ident)?;
                                    Ok(InboundPdu::Discarded { serial, body })
                                }
                            }
                        })
                        .await?;
                    match inbound {
                        Ok(InboundPdu::DecodedLegacy { decoded, effect }) => {
                            let (decoded_serial, payload) = decoded.into_parts();
                            log::debug!(
                                "decoded codec-46 serial {} {}",
                                decoded_serial,
                                payload.pdu_name()
                            );
                            if decoded_serial == 0 {
                                let MuxWireDecodedPayload::Pdu(pdu) = payload else {
                                    bail!(
                                        "codec-46 unilateral frame decoded to non-routable {}",
                                        payload.pdu_name()
                                    );
                                };
                                let decoded = DecodedPdu {
                                    serial: decoded_serial,
                                    pdu,
                                };
                                match topology.on_unilateral(decoded)? {
                                    ClientTopologyUnilateralAction::Buffered => {}
                                    ClientTopologyUnilateralAction::Route(routed) => {
                                        route_client_unilateral_batch(
                                            dispatch_authority,
                                            generation,
                                            &readiness,
                                            &mut pre_ready_unilateral,
                                            routed,
                                        )?;
                                    }
                                }
                                continue;
                            }

                            let serial = NonZeroU64::new(decoded_serial)
                                .expect("the unilateral serial-zero branch was handled above");
                            let effect = effect.ok_or_else(|| {
                                anyhow!(
                                    "decoded codec-46 RPC serial {} without exact response correlation",
                                    serial
                                )
                            })?;
                            let binding = pending.binding_for_correlated_response(serial);
                            match payload {
                                MuxWireDecodedPayload::Pdu(pdu) => {
                                    if effect != PendingRpcEffect::Ordinary {
                                        bail!(
                                            "codec-46 topology fence received unexpected {}",
                                            pdu.pdu_name()
                                        );
                                    }
                                    rpc_transport
                                        .complete_protocol_response(
                                            generation,
                                            binding.request,
                                            &pdu,
                                        )
                                        .map_err(NotReconnectableError::ProtocolViolation)?;
                                    pending.complete(serial, PendingRpcReply::pdu(pdu))?;
                                }
                                MuxWireDecodedPayload::Legacy46ListPanesResponse(response) => {
                                    if effect != PendingRpcEffect::LegacyTopologyFence {
                                        bail!(
                                            "codec-46 topology response matched an ordinary RPC serial {}",
                                            serial
                                        );
                                    }
                                    topology.on_legacy_response(serial)?;
                                    let completion = pending.complete(
                                        serial,
                                        PendingRpcReply::Legacy46ListPanesResponse {
                                            response,
                                            authority: LegacyTopologyFenceAuthority {
                                                generation,
                                                serial,
                                            },
                                        },
                                    )?;
                                    if completion.disposition == ReplyDisposition::Abandoned {
                                        bail!(
                                            "codec-46 topology snapshot consumer abandoned generation {} before local commit",
                                            generation
                                        );
                                    }
                                }
                                MuxWireDecodedPayload::Legacy46Rejection(rejection) => {
                                    if effect == PendingRpcEffect::CoherentTopologyFence {
                                        bail!(
                                            "codec-46 rejection crossed a current coherent topology fence"
                                        );
                                    }
                                    if effect == PendingRpcEffect::LegacyTopologyFence {
                                        let routed = topology.on_legacy_rejection(serial)?;
                                        route_client_unilateral_batch(
                                            dispatch_authority,
                                            generation,
                                            &readiness,
                                            &mut pre_ready_unilateral,
                                            routed,
                                        )?;
                                    }
                                    pending.complete(
                                        serial,
                                        PendingRpcReply::Legacy46Rejection(rejection),
                                    )?;
                                }
                                MuxWireDecodedPayload::Legacy46SendPaste(_) => {
                                    bail!(
                                        "codec-46 client received a server-side SendPaste request"
                                    );
                                }
                                MuxWireDecodedPayload::Unsupported(unsupported) => {
                                    bail!(
                                        "codec-46 frame {} is unsupported: {:?}",
                                        unsupported.ident(),
                                        unsupported.reason()
                                    );
                                }
                            }
                        }
                        Ok(InboundPdu::Decoded { decoded, effect }) => {
                            log::debug!(
                                "decoded serial {} {}",
                                decoded.serial,
                                decoded.pdu.pdu_name()
                            );
                            if decoded.serial == 0 {
                                match topology.on_unilateral(decoded)? {
                                    ClientTopologyUnilateralAction::Buffered => {}
                                    ClientTopologyUnilateralAction::Route(routed) => {
                                        route_client_unilateral_batch(
                                            dispatch_authority,
                                            generation,
                                            &readiness,
                                            &mut pre_ready_unilateral,
                                            routed,
                                        )?;
                                    }
                                }
                            } else {
                                let serial = NonZeroU64::new(decoded.serial)
                                    .expect("the unilateral serial-zero branch was handled above");
                                let effect = effect.ok_or_else(|| {
                                    anyhow!(
                                        "decoded RPC serial {} without exact response correlation",
                                        serial
                                    )
                                })?;
                                let binding = pending.binding_for_correlated_response(serial);
                                let topology_action =
                                    if effect == PendingRpcEffect::CoherentTopologyFence {
                                        Some(topology.on_response(serial, &decoded.pdu)?)
                                    } else {
                                        if matches!(
                                            &decoded.pdu,
                                            Pdu::ListPanesCoherentResponse(_)
                                        ) {
                                            bail!(
                                                "coherent topology response matched an ordinary \
                                                 RPC serial {}",
                                                serial
                                            );
                                        }
                                        None
                                    };
                                let (awaiting_commit, terminal_reason) = match topology_action {
                                    Some(ClientTopologyResponseAction::Route(routed)) => {
                                        route_client_unilateral_batch(
                                            dispatch_authority,
                                            generation,
                                            &readiness,
                                            &mut pre_ready_unilateral,
                                            routed,
                                        )?;
                                        (false, None)
                                    }
                                    Some(ClientTopologyResponseAction::AwaitCommit) => (true, None),
                                    Some(
                                        ClientTopologyResponseAction::TerminalAfterDelivery(
                                            reason,
                                        ),
                                    ) => (false, Some(reason)),
                                    None => (false, None),
                                };
                                rpc_transport
                                    .complete_protocol_response(
                                        generation,
                                        binding.request,
                                        &decoded.pdu,
                                    )
                                    .map_err(NotReconnectableError::ProtocolViolation)?;
                                if awaiting_commit {
                                    rpc_transport
                                        .establish_protocol_capabilities(
                                            generation,
                                            TopologyCapabilities::FENCED_SNAPSHOT_V1,
                                        )
                                        .map_err(NotReconnectableError::ProtocolViolation)?;
                                }
                                let completion = pending
                                    .complete(serial, PendingRpcReply::pdu(decoded.pdu))?;
                                if awaiting_commit
                                    && completion.disposition == ReplyDisposition::Abandoned
                                {
                                    bail!(
                                        "coherent topology snapshot consumer abandoned generation \
                                         {} before exact commit",
                                        generation
                                    );
                                }
                                if let Some(reason) = terminal_reason {
                                    bail!("{reason}");
                                }
                            }
                        }
                        Ok(InboundPdu::Discarded { serial, body }) => {
                            log::debug!(
                                "discarded {} encoded bytes in {} bounded-size chunks for abandoned RPC serial {}",
                                body.encoded_bytes(),
                                body.chunk_reads(),
                                serial,
                            );
                        }
                        Err(err) => {
                            pending.record_decode_protocol_error(&err);
                            log::error!("Error while decoding response pdu: {err:#}");
                            return Err(err).context("Error while decoding response pdu");
                        }
                    }
                }
                NextEvent::Readable(Err(err)) => {
                    let reason = format!("Error while waiting for stream readability: {:#}", err);
                    log::error!("{}", reason);
                    return Err(err).context("Error while waiting for stream readability");
                }
            }
        }
    }
    .await;

    if let Err(error) = &result {
        readiness.complete_error(&format!(
            "mux RPC readiness terminated before publication: {error:#}"
        ));
        if dispatch_authority.rpc_transport.terminal_error().is_none() {
            if let Err(retirement_error) = dispatch_authority.begin_rpc_transport_retirement() {
                log::error!(
                    "failed to retire mux client RPC transport after terminal reader result: \
                     {retirement_error:#}"
                );
            }
        }
        pending.fail_after_transport_error(error);
    }
    result
}

struct UnixConnectStream {
    stream: Option<UnixStream>,
    proxy_child: Option<std::process::Child>,
}

impl UnixConnectStream {
    fn direct(stream: UnixStream) -> Self {
        Self {
            stream: Some(stream),
            proxy_child: None,
        }
    }

    fn proxy(stream: UnixStream, child: std::process::Child) -> Self {
        Self {
            stream: Some(stream),
            proxy_child: Some(child),
        }
    }

    fn stream(&self) -> &UnixStream {
        self.stream
            .as_ref()
            .expect("unix connect stream accessed while dropping")
    }

    fn stream_mut(&mut self) -> &mut UnixStream {
        self.stream
            .as_mut()
            .expect("unix connect stream accessed while dropping")
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream().set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream().set_write_timeout(timeout)
    }
}

impl std::fmt::Debug for UnixConnectStream {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("UnixConnectStream")
            .field(
                "proxy_pid",
                &self.proxy_child.as_ref().map(std::process::Child::id),
            )
            .finish_non_exhaustive()
    }
}

fn terminate_and_reap_proxy_child(
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }

    match child.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::InvalidInput => {
            // The process exited between `try_wait` and `kill`; `wait` below
            // remains the single reaping authority.
        }
        Err(error) => return Err(error),
    }
    child.wait()
}

impl Drop for UnixConnectStream {
    fn drop(&mut self) {
        // Closing the socketpair first gives a cooperative proxy an immediate
        // EOF. We still terminate and synchronously reap the exact child we
        // spawned: `std::process::Child` does neither on Drop, so a rejected
        // attach candidate otherwise leaves a live SSH process and remote mux
        // connection behind for the lifetime of the GUI.
        drop(self.stream.take());
        if let Some(mut child) = self.proxy_child.take() {
            let pid = child.id();
            if let Err(error) = terminate_and_reap_proxy_child(&mut child) {
                log::error!("failed to terminate and reap unix proxy child {pid}: {error}");
            }
        }
    }
}

impl Read for UnixConnectStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Read::read(self.stream_mut(), buf)
    }
}

impl Write for UnixConnectStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Write::write(self.stream_mut(), buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(self.stream_mut())
    }
}

impl AsyncRead for UnixConnectStream {
    fn poll_read(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncRead::poll_read(Pin::new(self.get_mut().stream_mut()), task_cx, buf)
    }
}

impl AsyncWrite for UnixConnectStream {
    fn poll_write(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(self.get_mut().stream_mut()), task_cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write_vectored(Pin::new(self.get_mut().stream_mut()), task_cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        AsyncWrite::is_write_vectored(self.stream())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(self.get_mut().stream_mut()), task_cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(self.get_mut().stream_mut()), task_cx)
    }
}

fn unix_connect_with_retry(
    target: &UnixTarget,
    just_spawned: bool,
    max_attempts: Option<u64>,
) -> anyhow::Result<UnixConnectStream> {
    let mut error = None;

    if just_spawned {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let max_attempts = max_attempts.unwrap_or(10);
    if max_attempts == 0 {
        bail!("unix connection retry count must be greater than zero");
    }

    for iter in 0..max_attempts {
        if iter > 0 {
            std::thread::sleep(std::time::Duration::from_millis(iter * 50));
        }
        match target {
            UnixTarget::Socket(path) => match unix_stream_connect_with_timeout(
                path.to_path_buf(),
                UNIX_SOCKET_CONNECT_TIMEOUT,
            ) {
                Ok(stream) => return Ok(UnixConnectStream::direct(stream)),
                Err(err) => {
                    error =
                        Some(Err(err).with_context(|| format!("connecting to {}", path.display())))
                }
            },
            UnixTarget::Proxy(argv) => {
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| anyhow!("unix proxy command is empty"))?;
                let mut cmd = std::process::Command::new(program);
                cmd.args(args);

                let (a, b) = filedescriptor::socketpair()?;

                cmd.stdin(b.as_stdio()?);
                cmd.stdout(b.as_stdio()?);
                cmd.stderr(std::process::Stdio::inherit());
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning proxy command {:?}", cmd))?;

                error.take();

                // Grace period to detect whether connection failed
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            error = Some(Err(anyhow!(
                                "{:?} exited already with status {:?}",
                                cmd,
                                status
                            )));
                            continue;
                        }
                        Ok(None) => {
                            error.take();
                        }
                        Err(err) => {
                            error =
                                Some(Err(err).context(format!("spawning proxy command {:?}", cmd)));
                            continue;
                        }
                    }
                }

                if error.is_none() {
                    #[cfg(unix)]
                    unsafe {
                        use std::os::unix::io::{FromRawFd, IntoRawFd};
                        return Ok(UnixConnectStream::proxy(
                            UnixStream::from_raw_fd(a.into_raw_fd()),
                            child,
                        ));
                    }
                    #[cfg(windows)]
                    unsafe {
                        // `a` is a `filedescriptor::FileDescriptor`, which
                        // does not impl `std::os::windows::io::IntoRawSocket`
                        // directly — only the project's
                        // `IntoRawSocketDescriptor` (returning `SocketDescriptor
                        // = SOCKET`). `RawSocket` is also SOCKET-shaped on
                        // Windows, so the `as _` cast bridges them.
                        use filedescriptor::IntoRawSocketDescriptor;
                        use std::os::windows::io::FromRawSocket;
                        return Ok(UnixConnectStream::proxy(
                            UnixStream::from_raw_socket(a.into_socket_descriptor() as _),
                            child,
                        ));
                    }
                }

                if let Err(cleanup_error) = terminate_and_reap_proxy_child(&mut child) {
                    log::error!(
                        "failed to clean up rejected unix proxy child {}: {cleanup_error}",
                        child.id()
                    );
                }
            }
        }
    }

    error.unwrap_or_else(|| Err(anyhow!("unix connection failed without recording a cause")))
}

fn unix_stream_connect_with_timeout(
    path: PathBuf,
    timeout: Duration,
) -> std::io::Result<UnixStream> {
    if timeout.is_zero() {
        return Err(std::io::Error::new(
            ErrorKind::TimedOut,
            "unix socket connect timeout must be greater than zero",
        ));
    }

    let display_path = path.display().to_string();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("frankenterm-unix-connect-{}", std::process::id()))
        .spawn(move || {
            let _ = tx.send(UnixStream::connect(path));
        })
        .map_err(|err| {
            std::io::Error::other(format!("spawn unix socket connect timeout thread: {err}"))
        })?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
            ErrorKind::TimedOut,
            format!(
                "timed out after {}ms connecting to {}",
                timeout.as_millis(),
                display_path
            ),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            format!("unix socket connect worker exited before connecting to {display_path}"),
        )),
    }
}

// `async_trait` keeps this trait object-safe by generating boxed `Future`
// returns. Those futures are already intrinsically must-use, and the macro's
// own annotation therefore triggers `double_must_use` under newer Clippy.
// Scope the compatibility allowance to this one macro-generated trait surface.
#[allow(
    clippy::double_must_use,
    reason = "async_trait duplicates the intrinsic must-use contract of its generated boxed future"
)]
#[async_trait]
pub trait AsyncReadAndWrite: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug + Send {
    async fn wait_for_readable(&self) -> anyhow::Result<()>;
}

#[async_trait]
impl AsyncReadAndWrite for UnixStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        UnixStream::wait_for_readable(self)
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl AsyncReadAndWrite for UnixConnectStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        self.stream().wait_for_readable().await.map_err(Into::into)
    }
}

#[async_trait]
impl AsyncReadAndWrite for AsyncSslStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        AsyncSslStream::wait_for_readable(self)
            .await
            .map_err(Into::into)
    }
}

#[async_trait]
impl AsyncReadAndWrite for SshStream {
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        SshStream::wait_for_readable(self).await.map_err(Into::into)
    }
}

#[derive(Debug)]
struct Reconnectable {
    config: ClientDomainConfig,
    stream: Option<Box<dyn AsyncReadAndWrite>>,
    tls_creds: Option<GetTlsCredsResponse>,
}

struct SshStream {
    stdin: FileDescriptor,
    stdout: FileDescriptor,
    read_registration: Mutex<Option<IoRegistration>>,
    write_registration: Mutex<Option<IoRegistration>>,
    readiness_metrics: SshReadinessMetrics,
    read_poll_fallback: SshPollFallback,
    write_poll_fallback: SshPollFallback,
}

struct SshReadinessMetrics {
    read: SshReadinessOperationMetrics,
    write: SshReadinessOperationMetrics,
    wake_without_readability: metrics::Counter,
}

struct SshReadinessOperationMetrics {
    registration: metrics::Counter,
    rearm: metrics::Counter,
    missing_cx: metrics::Counter,
    reactor_unavailable: metrics::Counter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshIoDirection {
    Read,
    Write,
}

impl SshReadinessOperationMetrics {
    fn new(operation: &'static str) -> Self {
        Self {
            registration: metrics::counter!(
                "mux.client.ssh_stream.readiness.registration.total",
                "operation" => operation,
            ),
            rearm: metrics::counter!(
                "mux.client.ssh_stream.readiness.rearm.total",
                "operation" => operation,
            ),
            missing_cx: metrics::counter!(
                "mux.client.ssh_stream.readiness.missing_cx.total",
                "operation" => operation,
            ),
            reactor_unavailable: metrics::counter!(
                "mux.client.ssh_stream.readiness.reactor_unavailable.total",
                "operation" => operation,
            ),
        }
    }
}

impl SshReadinessMetrics {
    fn new() -> Self {
        Self {
            read: SshReadinessOperationMetrics::new("read"),
            write: SshReadinessOperationMetrics::new("write"),
            wake_without_readability: metrics::counter!(
                "mux.client.ssh_stream.readiness.wake_without_readability.total",
                "operation" => "read",
            ),
        }
    }
}

#[derive(Debug, Error)]
enum SshReadinessAuthorityError {
    #[error("SSH stream {operation} readiness requires an active asupersync task context")]
    MissingContext { operation: &'static str },
    #[error("SSH stream {operation} readiness reactor is unavailable during {phase}: {source}")]
    ReactorUnavailable {
        operation: &'static str,
        phase: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl SshReadinessAuthorityError {
    fn into_io_error(self) -> std::io::Error {
        let kind = match &self {
            Self::MissingContext { .. } => std::io::ErrorKind::NotConnected,
            Self::ReactorUnavailable { source, .. } => source.kind(),
        };
        std::io::Error::new(kind, self)
    }

    fn reactor_unavailable(
        operation: &'static str,
        phase: &'static str,
        source: std::io::Error,
    ) -> std::io::Error {
        Self::ReactorUnavailable {
            operation,
            phase,
            source,
        }
        .into_io_error()
    }
}

impl std::fmt::Debug for SshStream {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(fmt, "SshStream {{...}}")
    }
}

impl Drop for SshStream {
    fn drop(&mut self) {
        self.read_poll_fallback.reset();
        self.write_poll_fallback.reset();
    }
}

fn lock_registration_mutex(
    registration: &Mutex<Option<IoRegistration>>,
) -> MutexGuard<'_, Option<IoRegistration>> {
    match registration.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            registration.clear_poison();
            poisoned.into_inner()
        }
    }
}

impl SshStream {
    fn new(mut stdin: FileDescriptor, mut stdout: FileDescriptor) -> std::io::Result<Self> {
        // `frankenterm_ssh::ExecResult` exposes socketpair endpoints. Keep
        // that invariant explicit: readiness uses a non-consuming socket peek
        // so it remains valid above macOS's select(2) FD_SETSIZE ceiling.
        stdin
            .set_non_blocking(true)
            .map_err(std::io::Error::other)?;
        stdout
            .set_non_blocking(true)
            .map_err(std::io::Error::other)?;
        Ok(Self {
            stdin,
            stdout,
            read_registration: Mutex::new(None),
            write_registration: Mutex::new(None),
            readiness_metrics: SshReadinessMetrics::new(),
            read_poll_fallback: SshPollFallback::new(),
            write_poll_fallback: SshPollFallback::new(),
        })
    }

    async fn wait_for_readable(&self) -> std::io::Result<()> {
        let _reset_on_drop = SshPollResetOnDrop(&self.read_poll_fallback);
        let mut waiting = false;
        poll_fn(|task_cx| {
            let readable = match self.stdout_is_readable() {
                Ok(readable) => readable,
                Err(error) => {
                    self.read_poll_fallback.reset();
                    return Poll::Ready(Err(error));
                }
            };
            if readable {
                self.read_poll_fallback.reset();
                return Poll::Ready(Ok(()));
            }

            if waiting {
                self.readiness_metrics.wake_without_readability.increment(1);
            }

            if let Err(error) = self.register_interest_for_read(task_cx) {
                self.read_poll_fallback.reset();
                return Poll::Ready(Err(error));
            }
            waiting = true;

            // Close the probe/register race. Data can become readable after
            // the first zero-time probe but before the reactor registration is
            // installed. The second probe lets that data complete this future
            // without depending on whether the reactor observed the edge.
            let readable = match self.stdout_is_readable() {
                Ok(readable) => readable,
                Err(error) => {
                    self.read_poll_fallback.reset();
                    return Poll::Ready(Err(error));
                }
            };
            if readable {
                self.read_poll_fallback.reset();
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn stdout_is_readable(&self) -> std::io::Result<bool> {
        self.stdout
            .socket_readable_now()
            .map_err(std::io::Error::other)
    }

    fn register_interest_for_read(&self, task_cx: &TaskContext<'_>) -> std::io::Result<()> {
        self.register_interest(SshIoDirection::Read, task_cx)
    }

    fn register_interest_for_write(&self, task_cx: &TaskContext<'_>) -> std::io::Result<()> {
        self.register_interest(SshIoDirection::Write, task_cx)
    }

    fn register_interest(
        &self,
        direction: SshIoDirection,
        task_cx: &TaskContext<'_>,
    ) -> std::io::Result<()> {
        let (desc, registration, interest, operation, readiness_metrics, _poll_fallback) =
            match direction {
                SshIoDirection::Read => (
                    &self.stdout,
                    &self.read_registration,
                    Interest::READABLE,
                    "read",
                    &self.readiness_metrics.read,
                    &self.read_poll_fallback,
                ),
                SshIoDirection::Write => (
                    &self.stdin,
                    &self.write_registration,
                    Interest::WRITABLE,
                    "write",
                    &self.readiness_metrics.write,
                    &self.write_poll_fallback,
                ),
            };
        let Some(current) = Cx::current() else {
            readiness_metrics.missing_cx.increment(1);
            return Err(SshReadinessAuthorityError::MissingContext { operation }.into_io_error());
        };

        let mut registration = lock_registration_mutex(registration);
        if let Some(existing) = registration.as_mut() {
            readiness_metrics.rearm.increment(1);
            match existing.rearm(interest, task_cx.waker()) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    readiness_metrics.reactor_unavailable.increment(1);
                    *registration = None;
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::Unsupported | std::io::ErrorKind::NotConnected
                    ) =>
                {
                    readiness_metrics.reactor_unavailable.increment(1);
                    *registration = None;
                }
                Err(source) => {
                    readiness_metrics.reactor_unavailable.increment(1);
                    return Err(SshReadinessAuthorityError::reactor_unavailable(
                        operation, "rearm", source,
                    ));
                }
            }
        }

        #[cfg(unix)]
        {
            match current.register_io(desc, interest) {
                Ok(new_registration) => {
                    if !new_registration.update_waker(task_cx.waker().clone()) {
                        readiness_metrics.reactor_unavailable.increment(1);
                        return Err(SshReadinessAuthorityError::reactor_unavailable(
                            operation,
                            "install_waker",
                            std::io::Error::new(
                                std::io::ErrorKind::NotConnected,
                                "I/O registration waker slot disappeared",
                            ),
                        ));
                    }
                    *registration = Some(new_registration);
                    readiness_metrics.registration.increment(1);
                    Ok(())
                }
                Err(source) => {
                    readiness_metrics.reactor_unavailable.increment(1);
                    Err(SshReadinessAuthorityError::reactor_unavailable(
                        operation, "register", source,
                    ))
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = (desc, interest);
            drop(registration);
            readiness_metrics.reactor_unavailable.increment(1);
            // asupersync 0.3.10 has no public Windows `register_io` seam for
            // this socket type. The fallback therefore uses exponential
            // per-operation backoff plus a process-global 500-poll/second
            // reservation cadence. Fleet size can increase readiness latency,
            // but cannot multiply idle sockets into one 1 ms timer each.
            if fallback_rewake(&current, task_cx, _poll_fallback) {
                Ok(())
            } else {
                Err(SshReadinessAuthorityError::reactor_unavailable(
                    operation,
                    "windows_timer_fallback",
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "Windows SSH readiness fallback has no live timer-generation authority",
                    ),
                ))
            }
        }
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.stdout.read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.stdin.flush()
    }
}

impl AsyncRead for SshStream {
    fn poll_read(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.stdout.read(buf.unfilled()) {
            Ok(read) => {
                this.read_poll_fallback.reset();
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_read(task_cx) {
                    this.read_poll_fallback.reset();
                    return Poll::Ready(Err(register_err));
                }
                // Close the syscall/register race even if a future reactor
                // backend changes from level-triggered to edge-triggered.
                match this.stdout.read(buf.unfilled()) {
                    Ok(read) => {
                        this.read_poll_fallback.reset();
                        buf.advance(read);
                        Poll::Ready(Ok(()))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Poll::Pending,
                    Err(err) => {
                        this.read_poll_fallback.reset();
                        Poll::Ready(Err(err))
                    }
                }
            }
            Err(err) => {
                this.read_poll_fallback.reset();
                Poll::Ready(Err(err))
            }
        }
    }
}

impl AsyncWrite for SshStream {
    fn poll_write(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.stdin.write(buf) {
            Ok(written) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Ok(written))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    this.write_poll_fallback.reset();
                    return Poll::Ready(Err(register_err));
                }
                match this.stdin.write(buf) {
                    Ok(written) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Ok(written))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Poll::Pending,
                    Err(err) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Err(err))
                    }
                }
            }
            Err(err) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match this.stdin.write_vectored(bufs) {
            Ok(written) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Ok(written))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    this.write_poll_fallback.reset();
                    return Poll::Ready(Err(register_err));
                }
                match this.stdin.write_vectored(bufs) {
                    Ok(written) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Ok(written))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Poll::Pending,
                    Err(err) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Err(err))
                    }
                }
            }
            Err(err) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Err(err))
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match this.stdin.flush() {
            Ok(()) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Ok(()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Err(register_err) = this.register_interest_for_write(task_cx) {
                    this.write_poll_fallback.reset();
                    return Poll::Ready(Err(register_err));
                }
                match this.stdin.flush() {
                    Ok(()) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Ok(()))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Poll::Pending,
                    Err(err) => {
                        this.write_poll_fallback.reset();
                        Poll::Ready(Err(err))
                    }
                }
            }
            Err(err) => {
                this.write_poll_fallback.reset();
                Poll::Ready(Err(err))
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _task_cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().write_poll_fallback.reset();
        Poll::Ready(Ok(()))
    }
}

impl Reconnectable {
    fn new(config: ClientDomainConfig, stream: Option<Box<dyn AsyncReadAndWrite>>) -> Self {
        Self {
            config,
            stream,
            tls_creds: None,
        }
    }

    fn tls_creds_path(&self) -> anyhow::Result<PathBuf> {
        let path = config::pki_dir()?.join(self.config.name());
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn tls_creds_ca_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("ca.pem"))
    }

    fn tls_creds_cert_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("cert.pem"))
    }

    fn take_stream(&mut self) -> Option<Box<dyn AsyncReadAndWrite>> {
        self.stream.take()
    }

    fn is_local(&mut self) -> bool {
        matches!(&self.config, ClientDomainConfig::Unix(_))
    }

    fn reconnectable(&mut self) -> bool {
        match &self.config {
            // A plain local unix socket only disconnects when its server dies,
            // so reconnecting can't preserve the set of tabs and would leave us
            // with confusing, inconsistent state. BUT when the unix domain uses
            // a proxy_command (our remote frankenterm-mux-server reached over
            // `ssh ... nc -U <sock>`), the *remote* mux is persistent: a dropped
            // ssh/nc pipe is a transient transport failure, and reconnecting
            // re-runs the proxy_command and re-syncs the exact same remote tabs.
            // So reconnect iff there is a proxy_command.
            ClientDomainConfig::Unix(unix) => unix.proxy_command.is_some(),
            ClientDomainConfig::Tls(_) => true,
            // It *does* make sense to reconnect with an ssh session, but we
            // need to grow some smarts about whether the disconnect was because
            // we sent CTRL-D to close the last session, or whether it was a network
            // level disconnect, because we will otherwise throw up authentication
            // dialogs that would be annoying
            ClientDomainConfig::Ssh(_) => false,
        }
    }

    fn connect(
        &mut self,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        match self.config.clone() {
            ClientDomainConfig::Unix(unix_dom) => {
                self.unix_connect(unix_dom, initial, ui, no_auto_start)
            }
            ClientDomainConfig::Tls(tls) => self.tls_connect(tls, initial, ui),
            ClientDomainConfig::Ssh(ssh) => self.ssh_connect(ssh, initial, ui),
        }
    }

    /// Build one command for the remote WezTerm-compatible CLI.
    ///
    /// `remote_wezterm_path` retains its upstream configuration name, but in
    /// FrankenTerm it names a binary that implements the inherited `cli`
    /// grammar. An explicit path is POSIX-shell quoted because the SSH
    /// transport executes a command string. The standalone `ft` binary does
    /// not implement that grammar, so the implicit path must remain the
    /// actually supported `wezterm` executable until a first-party headless
    /// proxy is implemented and proven.
    fn build_remote_wezterm_cli_command(
        remote_wezterm_path: &Option<String>,
        cli_args: &str,
    ) -> anyhow::Result<String> {
        let path = remote_wezterm_path.as_deref().unwrap_or("wezterm");
        let executable = shlex::try_quote(path)
            .context("remote_wezterm_path contains a byte that cannot be shell quoted")?;
        Ok(format!("exec {executable} cli {cli_args}"))
    }

    fn build_ssh_proxy_command(
        remote_wezterm_path: &Option<String>,
        override_proxy_command: Option<&str>,
        initial: bool,
    ) -> anyhow::Result<String> {
        if let Some(cmd) = override_proxy_command {
            Ok(cmd.to_string())
        } else {
            let cli_args = if initial {
                "--prefer-mux proxy"
            } else {
                "--prefer-mux --no-auto-start proxy"
            };
            Self::build_remote_wezterm_cli_command(remote_wezterm_path, cli_args)
        }
    }

    fn build_tls_creds_command(remote_wezterm_path: &Option<String>) -> anyhow::Result<String> {
        Self::build_remote_wezterm_cli_command(remote_wezterm_path, "tlscreds")
    }

    fn should_retry_tls_bootstrap_after_reuse_error(err: &anyhow::Error) -> bool {
        match err.root_cause().downcast_ref::<std::io::Error>() {
            Some(ioerr) => ioerr.kind() == std::io::ErrorKind::ConnectionRefused,
            None => true,
        }
    }

    fn ssh_connect(
        &mut self,
        ssh_dom: SshDomain,
        initial: bool,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<()> {
        let ssh_config = mux::ssh::ssh_domain_to_ssh_config(&ssh_dom)?;

        let sess = ssh_connect_with_ui(ssh_config, ui)?;
        let cmd = Self::build_ssh_proxy_command(
            &ssh_dom.remote_wezterm_path,
            ssh_dom.override_proxy_command.as_deref(),
            initial,
        )?;
        ui.output_str(&format!("Running: {}\n", cmd));
        log::debug!("going to run {}", cmd);

        let exec = wezterm_ssh::runtime::block_on(sess.exec(&cmd, None))?;

        let mut stderr = exec.stderr;
        std::thread::Builder::new()
            .name("ssh-proxy-stderr".to_string())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(len) = stderr.read(&mut buf) {
                    if len == 0 {
                        break;
                    } else {
                        let stderr = &buf[0..len];
                        log::error!("ssh stderr: {}", String::from_utf8_lossy(stderr));
                    }
                }
            })
            .context("spawn ssh proxy stderr reader thread")?;

        // This is a bit gross, but it helps to surface errors in running
        // the proxy, and prevents us from hanging forever after the process
        // has died
        let mut child = exec.child;
        std::thread::Builder::new()
            .name("ssh-proxy-waiter".to_string())
            .spawn(move || match child.wait() {
                Err(err) => log::error!("waiting on {} failed: {:#}", cmd, err),
                Ok(status) if !status.success() => log::error!("{}: {}", cmd, status),
                _ => {}
            })
            .context("spawn ssh proxy waiter thread")?;

        let stream: Box<dyn AsyncReadAndWrite> = Box::new(SshStream::new(exec.stdin, exec.stdout)?);
        self.stream.replace(stream);
        Ok(())
    }

    fn unix_connect(
        &mut self,
        unix_dom: UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        let target = unix_dom.target();
        ui.output_str(&format!("Connect to {:?}\n", target));
        log::trace!("connect to {:?}", target);

        let max_attempts = if no_auto_start { Some(1) } else { None };

        let stream = match unix_connect_with_retry(&target, false, max_attempts) {
            Ok(stream) => stream,
            Err(e) => {
                if no_auto_start || unix_dom.no_serve_automatically || !initial {
                    bail!("failed to connect to {:?}: {}", target, e);
                }
                log::warn!(
                    "While connecting to {:?}: {}.  Will try spawning the server.",
                    target,
                    e
                );
                ui.output_str(&format!("Error: {}.  Will try spawning server.\n", e));

                let argv = unix_dom.serve_command()?;
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| anyhow!("unix domain serve command is empty"))?;

                let mut cmd = std::process::Command::new(program);
                cmd.args(args);

                #[cfg(unix)]
                if let Some(mask) = umask::UmaskSaver::saved_umask() {
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::umask(mask);
                            Ok(())
                        });
                    }
                }

                log::warn!("Running: {:?}", cmd);
                ui.output_str(&format!("Running: {:?}\n", cmd));

                let child = cmd
                    .spawn()
                    .with_context(|| format!("while spawning {:?}", cmd))?;
                if let Err(err) = std::thread::Builder::new()
                    .name("unix-domain-server-waiter".to_string())
                    .spawn(move || match child.wait_with_output() {
                        Ok(out) => {
                            if let Ok(stdout) = std::str::from_utf8(&out.stdout) {
                                if !stdout.is_empty() {
                                    log::warn!("stdout: {}", stdout);
                                }
                            }
                            if let Ok(stderr) = std::str::from_utf8(&out.stderr) {
                                if !stderr.is_empty() {
                                    log::warn!("stderr: {}", stderr);
                                }
                            }
                        }
                        Err(err) => {
                            log::error!("spawn: {:#}", err);
                        }
                    })
                {
                    log::error!("failed to spawn unix domain server waiter thread: {err:#}");
                }

                unix_connect_with_retry(&target, true, None).with_context(|| {
                    format!("(after spawning server) failed to connect to {:?}", target)
                })?
            }
        };

        ui.output_str("Transport connected; protocol verification pending.\n");
        stream.set_read_timeout(Some(unix_dom.read_timeout))?;
        stream.set_write_timeout(Some(unix_dom.write_timeout))?;
        let stream: Box<dyn AsyncReadAndWrite> = Box::new(stream);
        self.stream.replace(stream);
        Ok(())
    }

    pub fn tls_connect(
        &mut self,
        tls_client: TlsDomainClient,
        _initial: bool,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<()> {
        openssl::init();

        let remote_address = &tls_client.remote_address;

        let remote_host_name = remote_address.split(':').next().ok_or_else(|| {
            anyhow!(
                "expected mux_server_remote_address to have the form 'host:port', but have {}",
                remote_address
            )
        })?;

        // If we are reconnecting and already bootstrapped via SSH, let's see if
        // we can connect using those same credentials and avoid running through
        // the SSH authentication flow.
        if let Some(Ok(_)) = tls_client.ssh_parameters() {
            match self.try_connect(&tls_client, ui, remote_address, remote_host_name) {
                Ok(stream) => {
                    self.stream.replace(stream);
                    return Ok(());
                }
                Err(err) => {
                    if !Self::should_retry_tls_bootstrap_after_reuse_error(&err) {
                        // Transport-level IO failures other than connection-refused
                        // mean we had trouble reaching or otherwise talking to the
                        // remote host. Re-running the SSH bootstrap is unlikely to help.
                        return Err(err);
                    }
                    ui.output_str(&format!(
                        "Failed to reuse creds: {:?}\nWill retry bootstrap via SSH\n",
                        err
                    ));
                }
            }
        }

        if let Some(Ok(ssh_params)) = tls_client.ssh_parameters() {
            if self.tls_creds.is_none() {
                // We need to bootstrap via an ssh session

                let mut ssh_config = wezterm_ssh::Config::new();
                ssh_config.add_default_config_files();

                let mut fields = ssh_params.host_and_port.split(':');
                let host = fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no host component somehow"))?;
                let port = fields.next();

                let mut ssh_config = ssh_config.for_host(host);
                if let Some(username) = &ssh_params.username {
                    ssh_config.insert("user".to_string(), username.to_string());
                }
                if let Some(port) = port {
                    ssh_config.insert("port".to_string(), port.to_string());
                }

                let sess = ssh_connect_with_ui(ssh_config, ui)?;

                let creds = ui.run_and_log_error(|| {
                    // The `tlscreds` command will start the server if needed and then
                    // obtain client credentials that we can use for tls.
                    let cmd = Self::build_tls_creds_command(&tls_client.remote_wezterm_path)?;

                    ui.output_str(&format!("Running: {}\n", cmd));
                    let mut exec = wezterm_ssh::runtime::block_on(sess.exec(&cmd, None))
                        .with_context(|| format!("executing `{}` on remote host", cmd))?;

                    log::debug!("waiting for command to finish");
                    let status = exec.child.wait()?;
                    if !status.success() {
                        anyhow::bail!("{} failed", cmd);
                    }

                    drop(exec.stdin);

                    let mut stderr = exec.stderr;
                    if let Err(err) = thread::Builder::new()
                        .name("tls-creds-stderr".to_string())
                        .spawn(move || {
                            // stderr is ideally empty
                            let mut err = String::new();
                            let _ = stderr.read_to_string(&mut err);
                            if !err.is_empty() {
                                log::error!("remote: `{}` stderr -> `{}`", cmd, err);
                            }
                        })
                    {
                        log::error!("failed to spawn tls creds stderr reader thread: {err:#}");
                    }

                    let creds = match Pdu::decode(exec.stdout)
                        .context("reading tlscreds response")?
                        .pdu
                    {
                        Pdu::GetTlsCredsResponse(creds) => creds,
                        _ => bail!("unexpected response to tlscreds"),
                    };

                    // Save the credentials to disk, as that is currently the easiest
                    // way to get them into openssl.  Ideally we'd keep these entirely
                    // in memory.
                    std::fs::write(&self.tls_creds_ca_path()?, creds.ca_cert_pem.as_bytes())?;
                    std::fs::write(
                        &self.tls_creds_cert_path()?,
                        creds.client_cert_pem.as_bytes(),
                    )?;
                    log::info!("got TLS creds");
                    Ok(creds)
                })?;
                self.tls_creds.replace(creds);
            }
        }

        let cloned_ui = ui.clone();
        let stream = cloned_ui.run_and_log_error({
            || self.try_connect(&tls_client, ui, remote_address, remote_host_name)
        })?;
        self.stream.replace(stream);
        Ok(())
    }

    fn try_connect(
        &mut self,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
        remote_address: &str,
        remote_host_name: &str,
    ) -> anyhow::Result<Box<dyn AsyncReadAndWrite>> {
        let mut connector = SslConnector::builder(SslMethod::tls())?;

        let cert_file = match tls_client.pem_cert.clone() {
            Some(cert) => cert,
            None => self.tls_creds_cert_path()?,
        };

        connector
            .set_certificate_file(&cert_file, SslFiletype::PEM)
            .context(format!(
                "set_certificate_file to {} for TLS client",
                cert_file.display()
            ))?;

        if let Some(chain_file) = tls_client.pem_ca.as_ref() {
            connector
                .set_certificate_chain_file(chain_file)
                .context(format!(
                    "set_certificate_chain_file to {} for TLS client",
                    chain_file.display()
                ))?;
        }

        let key_file = match tls_client.pem_private_key.clone() {
            Some(key) => key,
            None => self.tls_creds_cert_path()?,
        };
        connector
            .set_private_key_file(&key_file, SslFiletype::PEM)
            .context(format!(
                "set_private_key_file to {} for TLS client",
                key_file.display()
            ))?;

        fn load_cert(name: &Path) -> anyhow::Result<X509> {
            let cert_bytes = std::fs::read(name)?;
            log::trace!("loaded {}", name.display());
            Ok(X509::from_pem(&cert_bytes)?)
        }
        for name in &tls_client.pem_root_certs {
            if name.is_dir() {
                for entry in std::fs::read_dir(name)? {
                    if let Ok(cert) = load_cert(&entry?.path()) {
                        connector.cert_store_mut().add_cert(cert).ok();
                    }
                }
            } else {
                connector.cert_store_mut().add_cert(load_cert(name)?)?;
            }
        }

        if let Ok(ca_path) = self.tls_creds_ca_path() {
            if ca_path.exists() {
                connector.cert_store_mut().add_cert(load_cert(&ca_path)?)?;
            }
        }

        let connector = connector.build();
        let connector = connector
            .configure()?
            .verify_hostname(!tls_client.accept_invalid_hostnames);

        ui.output_str(&format!("Connecting to {} using TLS\n", remote_address));
        let stream = TcpStream::connect(remote_address)
            .with_context(|| format!("connecting to {}", remote_address))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(tls_client.write_timeout))?;
        stream.set_read_timeout(Some(tls_client.read_timeout))?;

        let stream = Box::new(AsyncSslStream::new(
            connector
                .connect(
                    tls_client
                        .expected_cn
                        .as_deref()
                        .unwrap_or(remote_host_name),
                    stream,
                )
                .with_context(|| {
                    format!(
                        "SslConnector for {} with host name {}",
                        remote_address, remote_host_name,
                    )
                })?,
        ));
        ui.output_str("TLS transport connected; protocol verification pending.\n");
        Ok(stream)
    }
}

#[must_use]
const fn reconnect_cycle_budget_exhausted(max_attempts: u32, failed_cycles: u32) -> bool {
    max_attempts != 0 && failed_cycles > max_attempts
}

#[must_use]
const fn reconnect_dial_budget_exhausted(max_attempts: u32, dial_attempts: u32) -> bool {
    max_attempts != 0 && dial_attempts >= max_attempts
}

#[must_use]
fn next_reconnect_backoff(backoff: Duration, max_interval: Duration) -> Duration {
    backoff.saturating_mul(2).min(max_interval)
}

/// Enforce reconnect cadence independently of the optional rendering UI.
///
/// A connection window can be closed by the operator or can lose its render
/// task.  In that case `ConnectionUI::sleep_with_reason` fails immediately;
/// treating the UI as the timer would turn the default unbounded retry policy
/// into a tight dial loop.  Measure the UI attempt and park the reconnect OS
/// thread for any remaining interval on both success and failure.
fn wait_for_reconnect_backoff(
    duration: Duration,
    ui_sleep: impl FnOnce(Duration) -> anyhow::Result<()>,
) -> Option<anyhow::Error> {
    let started = std::time::Instant::now();
    let result = ui_sleep(duration);
    let remaining = duration.saturating_sub(started.elapsed());
    if !remaining.is_zero() {
        std::thread::sleep(remaining);
    }
    result.err()
}

impl Client {
    fn matches_dispatch_authority(&self, authority: &ClientDispatchAuthority) -> bool {
        Arc::ptr_eq(&self.incarnation, &authority.client_incarnation)
            && Arc::ptr_eq(
                &self.connection_generation,
                &authority.connection_generation,
            )
            && Arc::ptr_eq(&self.rpc_transport, &authority.rpc_transport)
    }

    fn new(
        local_domain_id: Option<DomainId>,
        mut reconnectable: Reconnectable,
        mux_owner: Weak<Mux>,
    ) -> Self {
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, mut receiver) = unbounded();
        let client_id = ClientId::new();
        let incarnation = Arc::new(ClientIncarnation);
        let connection_generation = Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION));
        let rpc_transport = Arc::new(RpcTransportState::new());
        let domain_reconnect_authorized = Arc::new(AtomicBool::new(local_domain_id.is_none()));
        let initial_dispatch_authority = ClientDispatchAuthority::new(
            local_domain_id,
            mux_owner,
            Arc::clone(&incarnation),
            Arc::clone(&connection_generation),
            Arc::clone(&rpc_transport),
        );
        let mut reconnect_dispatch_authority = initial_dispatch_authority.clone();
        let reconnect_authorization = Arc::clone(&domain_reconnect_authorized);

        if let Err(err) = thread::Builder::new()
            .name("client-reconnect".to_string())
            .spawn(move || {
                let cfg = configuration();
                let base_interval = Duration::from_millis(cfg.client_reconnect_base_interval_ms);
                let max_interval = Duration::from_millis(cfg.client_reconnect_max_interval_ms);

                let max_attempts = cfg.client_reconnect_max_attempts;
                let healthy_session =
                    Duration::from_millis(cfg.client_reconnect_healthy_session_ms);

                let mut backoff = base_interval;
                // One connection window for the whole domain, opened lazily on
                // the first disconnect. It used to be constructed inside the
                // loop, so every reconnect cycle spawned a *new* window: a
                // host that accepts the connection and then immediately drops
                // the session cycles forever, and the user gets an endless
                // stream of "Reconnecting..." windows at startup.
                let mut reconnect_ui: Option<ConnectionUI> = None;
                // Cycles since the last connection that stayed up long enough
                // to count as recovered. Bounds the churn above.
                let mut failed_cycles: u32 = 0;
                loop {
                    let session_started = std::time::Instant::now();
                    let (thread_result, returned_reconnectable, returned_receiver) = client_thread(
                        reconnectable,
                        receiver,
                        reconnect_dispatch_authority.clone(),
                    );
                    reconnectable = returned_reconnectable;
                    receiver = returned_receiver;
                    let not_reconnectable = thread_result
                        .as_ref()
                        .err()
                        .and_then(|error| error.downcast_ref::<NotReconnectableError>())
                        .cloned();
                    if let Err(error) = &thread_result {
                        if let Some(terminal) = error
                            .downcast_ref::<RpcTransportError>()
                            .filter(|error| error.is_incarnation_terminal())
                        {
                            log::error!(
                                "{terminal}; closing this mux client incarnation without \
                                 publishing or dialing a successor generation"
                            );
                            break;
                        }
                    }
                    reconnect_dispatch_authority = match reconnect_dispatch_authority
                        .advance_generation(&receiver)
                    {
                        Ok(next) => next,
                        Err(err) => {
                            log::error!("cannot retire mux client transport authority: {err:#}");
                            break;
                        }
                    };
                    if let Some(terminal) = not_reconnectable {
                        // Revoking the retired generation is mandatory even
                        // when this incarnation will never reconnect.  Without
                        // that fence, work already bound to the dead transport
                        // can still observe its generation as current after
                        // the reconnect thread has closed.
                        log::error!("{terminal}; won't try to reconnect");
                        break;
                    }
                    if local_domain_id.is_some()
                        && !reconnect_authorization.load(AtomicOrdering::Acquire)
                    {
                        log::error!(
                            "initial client attachment ended before reconnect authority was published; closing this incarnation without dialing a successor"
                        );
                        break;
                    }
                    // A session that survived long enough is a genuine
                    // recovery, not churn: forget the earlier failures so a
                    // long-lived domain can still reconnect indefinitely
                    // across ordinary transient drops.
                    if session_started.elapsed() >= healthy_session {
                        failed_cycles = 0;
                    }
                    if let Err(e) = thread_result {
                        if !reconnectable.reconnectable() || local_domain_id.is_none() {
                            log::debug!("client thread ended: {}", e);
                            break;
                        }

                        let local_domain_id = local_domain_id.expect("checked above");

                        if let Some(ioerr) = e.root_cause().downcast_ref::<std::io::Error>() {
                            if let std::io::ErrorKind::UnexpectedEof = ioerr.kind() {
                                // A clean server shutdown surfaces as EOF; for a
                                // TLS or plain-local connection that means "stop,
                                // don't reconnect". But for a unix proxy_command
                                // domain (remote mux reached over ssh+nc) an EOF
                                // just means the transport pipe dropped while the
                                // remote mux keeps running, so we reconnect and
                                // re-sync rather than give up.
                                let is_unix_proxy = matches!(
                                    &reconnectable.config,
                                    ClientDomainConfig::Unix(u) if u.proxy_command.is_some()
                                );
                                if !is_unix_proxy {
                                    log::error!("server closed connection ({})", e);
                                    break;
                                }
                                log::warn!(
                                    "proxy_command connection closed ({}); will reconnect",
                                    e
                                );
                            }
                        }

                        failed_cycles = failed_cycles.saturating_add(1);
                        if reconnect_cycle_budget_exhausted(max_attempts, failed_cycles) {
                            log::error!(
                                "giving up on domain {local_domain_id}: {failed_cycles} \
                                 reconnect cycles without a session lasting {healthy_session:?} \
                                 (last error: {e}). Set \
                                 client_reconnect_max_attempts to 0 to keep retrying."
                            );
                            if let Some(ui) = reconnect_ui.as_ref() {
                                ui.output_str(&format!(
                                    "Giving up after {failed_cycles} reconnect attempts: {e}\n"
                                ));
                            }
                            break;
                        }

                        let ui = reconnect_ui.get_or_insert_with(|| {
                            let ui = ConnectionUI::new();
                            ui.title("FrankenTerm: Reconnecting...");
                            ui
                        });

                        // The default zero budget deliberately retries until
                        // recovery or app exit. Operators that explicitly set
                        // a nonzero budget bound this down-host dial loop as
                        // well as the rapid connect/drop cycles above.
                        let mut reconnected = false;
                        let mut dial_attempts: u32 = 0;
                        let mut ui_backoff_failure_reported = false;
                        loop {
                            let reason = format!("client disconnected {}; will reconnect", e);
                            if let Some(ui_error) =
                                wait_for_reconnect_backoff(backoff, |delay| {
                                    ui.sleep_with_reason(&reason, delay)
                                })
                            {
                                if !ui_backoff_failure_reported {
                                    ui_backoff_failure_reported = true;
                                    log::warn!(
                                        "reconnect UI is unavailable for domain {local_domain_id}; \
                                         continuing with mandatory non-UI backoff: {ui_error:#}"
                                    );
                                }
                            }
                            let initial = false;
                            let no_auto_start = true; // Don't auto-start on a reconnect
                            match reconnectable.connect(initial, ui, no_auto_start) {
                                Ok(_) => {
                                    if let Err(err) =
                                        reconnect_dispatch_authority.activate_rpc_transport()
                                    {
                                        log::error!(
                                            "cannot activate reconnected mux client transport: \
                                             {err:#}"
                                        );
                                        ui.output_str(&format!(
                                            "Transport reconnected, but domain {local_domain_id} remains unavailable because the successor RPC transport could not be activated: {err}\n"
                                        ));
                                        break;
                                    }
                                    backoff = base_interval;
                                    log::info!(
                                        "reconnected domain {local_domain_id} transport; verifying codec and reattaching topology"
                                    );
                                    let reattach_ui = ui.clone();
                                    match reconnect_dispatch_authority.resolve_current() {
                                        Ok(Some(dispatch)) => {
                                            let rpc = dispatch.bootstrap_rpc_scope();
                                            match reserve_client_main_thread(
                                                MainThreadServiceClass::Topology,
                                                CLIENT_MAIN_THREAD_TOPOLOGY_ESTIMATED_BYTES,
                                                "reconnect reattach",
                                            ) {
                                                Ok(reservation) => {
                                                    reservation
                                                        .spawn(async move {
                                                            if !dispatch.rpc_generation_is_live() {
                                                                return;
                                                            }
                                                            let result_ui = reattach_ui.clone();
                                                            let result =
                                                                ClientDomain::reattach_if_current(
                                                                    Arc::clone(&dispatch.mux),
                                                                    &dispatch.domain,
                                                                    Arc::clone(&dispatch.inner),
                                                                    rpc,
                                                                    reattach_ui,
                                                                )
                                                                .await;
                                                            if let Err(err) = result {
                                                                log::error!(
                                                                    "reconnected mux client reattach failed: {err:#}"
                                                                );
                                                                result_ui.output_str(&format!(
                                                                    "Transport reconnected, but domain {local_domain_id} remains unavailable because codec/topology reattach failed: {err}\n"
                                                                ));
                                                                return;
                                                            }
                                                            if !dispatch.rpc_generation_is_live() {
                                                                return;
                                                            }
                                                            log::info!(
                                                                "reconnected and reattached domain {local_domain_id}"
                                                            );
                                                            result_ui.output_str(&format!(
                                                                "Reconnected and reattached domain {local_domain_id}.\n"
                                                            ));
                                                        })
                                                        .detach();
                                                    reconnected = true;
                                                }
                                                Err(err) => {
                                                    log::error!(
                                                        "cannot schedule reconnect reattach for domain {local_domain_id}: {err:#}"
                                                    );
                                                    match dispatch.inner.client
                                                        .abort_rpc_transport_generation(
                                                            &rpc,
                                                            "successor topology scheduler admission failed",
                                                        )
                                                    {
                                                        Ok(()) => {
                                                            reattach_ui.output_str(&format!(
                                                                "Transport reconnected, but domain {local_domain_id} remains unavailable because topology reattach could not be scheduled: {err}. The exact successor generation was fenced and will be retried.\n"
                                                            ));
                                                            // Let the aborted reader enter the
                                                            // ordinary retirement path so it can
                                                            // mint a fresh transport generation.
                                                            reconnected = true;
                                                        }
                                                        Err(abort_err) => {
                                                            reattach_ui.output_str(&format!(
                                                                "Transport reconnected, but domain {local_domain_id} remains unavailable because topology reattach could not be scheduled ({err}) and the exact successor generation could not be fenced ({abort_err}).\n"
                                                            ));
                                                            log::error!(
                                                                "cannot fence reconnect generation after topology scheduler rejection for domain {local_domain_id}: {abort_err:#}"
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Ok(None) => {
                                            log::error!(
                                                "closing reconnected transport for domain \
                                                 {local_domain_id}: no exact published client \
                                                 attachment owns the successor generation"
                                            );
                                            reattach_ui.output_str(&format!(
                                                "Transport reconnected, but domain {local_domain_id} remains unavailable because no exact successor attachment owns the new connection.\n"
                                            ));
                                        }
                                        Err(err) => {
                                            log::error!(
                                                "cannot resolve reconnect reattach authority for \
                                                 domain {local_domain_id}: {err:#}"
                                            );
                                            reattach_ui.output_str(&format!(
                                                "Transport reconnected, but domain {local_domain_id} remains unavailable because reattach authority resolution failed: {err}\n"
                                            ));
                                        }
                                    }
                                    break;
                                }
                                Err(err) => {
                                    dial_attempts = dial_attempts.saturating_add(1);
                                    if reconnect_dial_budget_exhausted(
                                        max_attempts,
                                        dial_attempts,
                                    ) {
                                        ui.output_str(&format!(
                                            "giving up after {dial_attempts} attempts: {err}\n"
                                        ));
                                        break;
                                    }
                                    backoff = next_reconnect_backoff(backoff, max_interval);
                                    ui.output_str(&format!(
                                        "problem reconnecting: {}; will reconnect in {:?}\n",
                                        err, backoff
                                    ));
                                }
                            }
                        }
                        if !reconnected {
                            log::error!(
                                "giving up on domain {local_domain_id}: could not reconnect \
                                 after {dial_attempts} attempts (last error: {e}). Set \
                                 client_reconnect_max_attempts to 0 to keep retrying."
                            );
                            break;
                        }
                    } else {
                        log::error!("client_thread returned without any error condition");
                        break;
                    }
                }

                reconnect_dispatch_authority
                    .close_rpc_transport(&receiver, "mux client reconnect loop terminated");
                match reconnect_dispatch_authority.resolve_current() {
                    Ok(Some(dispatch)) => {
                        // Closing transport authority is already serialized by
                        // the exact dispatch generation. Logical domain
                        // retirement and the mux's deferred cleanup worker are
                        // thread-safe; neither may depend on a GUI scheduler
                        // that can be permanently retired during shutdown or
                        // reconfiguration. Taking the exact attachment slot
                        // here also makes Domain::state truthful immediately,
                        // allowing desired-state reconciliation to retry.
                        if dispatch.is_current() {
                            let client_domain = dispatch.client_domain();
                            if dispatch.is_current() {
                                let _ =
                                    client_domain.perform_detach_if_current(&dispatch.inner);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if let Some(domain_id) = local_domain_id {
                            log::error!(
                                "cannot resolve final detach authority for domain \
                                 {domain_id}: {err:#}"
                            );
                        } else {
                            log::error!("cannot resolve final standalone authority: {err:#}");
                        }
                    }
                }
            })
        {
            log::error!("failed to spawn client reconnect thread: {err:#}");
            initial_dispatch_authority.close_rpc_transport_without_receiver();
            let _ = initial_dispatch_authority
                .connection_generation
                .compare_exchange(
                    initial_dispatch_authority.generation,
                    0,
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Acquire,
                );
        }

        Self {
            sender,
            local_domain_id,
            incarnation,
            connection_generation,
            rpc_transport,
            domain_reconnect_authorized,
            is_reconnectable,
            is_local,
            client_id,
            client_domain_config,
        }
    }

    pub fn into_client_domain_config(self) -> ClientDomainConfig {
        self.client_domain_config
    }

    pub(crate) fn authorize_domain_reconnect(&self) {
        self.domain_reconnect_authorized
            .store(true, AtomicOrdering::Release);
    }

    pub(crate) fn revoke_domain_reconnect(&self) {
        self.domain_reconnect_authorized
            .store(false, AtomicOrdering::Release);
    }

    pub async fn verify_version_compat(
        &self,
        ui: &ConnectionUI,
    ) -> anyhow::Result<GetCodecVersionResponse> {
        let rpc = self.bootstrap_rpc_scope();
        let mut abort_guard =
            rpc.abort_guard("standalone mux RPC bootstrap failed, timed out, or was cancelled")?;
        let result = with_mux_rpc_bootstrap_timeout(async {
            let info = self.verify_version_compat_with_scope(ui, &rpc).await?;
            self.publish_rpc_transport_ready(&rpc, &abort_guard).await?;
            Ok(info)
        })
        .await;
        if result.is_ok() {
            abort_guard.disarm();
        }
        result
    }

    pub(crate) async fn verify_version_compat_with_scope(
        &self,
        ui: &ConnectionUI,
        rpc: &RpcGenerationScope,
    ) -> anyhow::Result<GetCodecVersionResponse> {
        let version_info = {
            let codec_version = rpc.get_codec_version(GetCodecVersion {});
            let timeout = async {
                promise::spawn::sleep(Duration::from_secs(60)).await;
                Err(Timeout).context("Timeout")
            };

            pin_mut!(codec_version);
            pin_mut!(timeout);

            match select(codec_version, timeout).await {
                Either::Left((result, _)) => result,
                Either::Right((result, _)) => result,
            }
        };

        match version_info {
            Ok(info) => {
                let generation = rpc.connection_generation().ok_or_else(|| {
                    anyhow!("codec negotiation completed for an unavailable RPC scope")
                })?;
                let codec = match RpcCodecAuthority::negotiate(
                    generation,
                    info.codec_vers,
                    info.min_supported,
                ) {
                    Ok(codec) => codec,
                    Err(compat_error) => {
                        let err = IncompatibleVersionError {
                            version: info.version_string,
                            codec_vers: info.codec_vers,
                            remote_min_supported: compat_error.remote_min,
                        };
                        ui.output_str(&err.to_string());
                        log::error!("{:?}", err);
                        return Err(err.into());
                    }
                };
                if !codec.dialect.is_legacy46() && codec.agreed < TOPOLOGY_FENCE_MIN_CODEC_VERSION {
                    let error = MissingTopologyFenceProtocolError {
                        remote_codec_version: info.codec_vers,
                        minimum_codec_version: TOPOLOGY_FENCE_MIN_CODEC_VERSION,
                    };
                    ui.output_str(&error.to_string());
                    log::error!("{error}");
                    return Err(error.into());
                }
                rpc.retain_codec_authority(codec)
                    .context("retaining agreed codec authority for the live RPC generation")?;
                let codec = rpc.codec_authority().ok_or_else(|| {
                    anyhow!(
                        "RPC generation {} retired while retaining codec authority",
                        generation
                    )
                })?;
                if codec.dialect.is_legacy46() {
                    metrics::counter!(
                        "mux.client.legacy46_connection.total",
                        "outcome" => "degraded_safe"
                    )
                    .increment(1);
                    let warning = format!(
                        "Connected to legacy codec-46 mux server {} in degraded-safe mode: tiled topology and text terminal I/O are available; floating-pane state is unavailable and will be preserved locally rather than treated as empty.\n",
                        info.version_string
                    );
                    ui.output_str(&warning);
                    log::warn!("{}", warning.trim_end());
                }
                if info.codec_vers != CODEC_VERSION {
                    log::warn!(
                        "Codec compat window: server={}, client={}, agreed={} \
                         (peer is inside the supported window)",
                        info.codec_vers,
                        CODEC_VERSION,
                        codec.agreed
                    );
                }
                log::trace!(
                    "Server version is {} (local codec window {}..={}, remote window {}..={}, \
                     agreed {}, generation {})",
                    info.version_string,
                    codec.local_min,
                    codec.local_max,
                    codec.remote_min,
                    codec.remote_max,
                    codec.agreed,
                    codec.generation,
                );
                rpc.set_client_id(SetClientId {
                    client_id: self.client_id.clone(),
                    is_proxy: false,
                })
                .await?;
                Ok(info)
            }
            Err(err) => {
                log::trace!("{:?}", err);
                let msg = if err.root_cause().is::<Timeout>() {
                    "Timed out while parsing the response from the server. \
                    This may be due to network connectivity issues"
                        .to_string()
                } else if err.root_cause().is::<CorruptResponse>() {
                    "Received an implausible and likely corrupt response from \
                    the server. This can happen if the remote host outputs \
                    to stdout prior to running commands. \
                    Check your shell startup!"
                        .to_string()
                } else if err.root_cause().is::<RpcTransportError>() {
                    format!(
                        "The mux transport retired while checking the server version: {err}. \
                         Reconnect and retry the explicit attach operation."
                    )
                } else {
                    format!(
                        "Please install the same version of FrankenTerm on both \
                     the client and server! \
                     The server reported error '{err}' while being asked for its \
                     version.  This likely means that the server is older \
                     than the client, but it could also happen if the remote \
                     host outputs to stdout prior to running commands. \
                     Check your shell startup!",
                    )
                };
                ui.output_str(&msg);
                bail!("{}", msg);
            }
        }
    }

    #[allow(dead_code)]
    pub fn local_domain_id(&self) -> Option<DomainId> {
        self.local_domain_id
    }

    fn compute_unix_domain(
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<config::UnixDomain> {
        match std::env::var_os("WEZTERM_UNIX_SOCKET") {
            Some(path) if !path.is_empty() => Ok(config::UnixDomain {
                socket_path: Some(path.into()),
                ..Default::default()
            }),
            Some(_) | None => {
                if !prefer_mux {
                    if let Ok(gui) = crate::discovery::resolve_gui_sock_path(class_name) {
                        return Ok(config::UnixDomain {
                            socket_path: Some(gui),
                            no_serve_automatically: true,
                            ..Default::default()
                        });
                    }
                }

                let config = configuration();
                Ok(config
                    .unix_domains
                    .first()
                    .ok_or_else(|| {
                        anyhow!(
                            "no default unix domain is configured and WEZTERM_UNIX_SOCKET \
                             is not set in the environment"
                        )
                    })?
                    .clone())
            }
        }
    }

    pub fn new_default_unix_domain(
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<Self> {
        let unix_dom = Self::compute_unix_domain(prefer_mux, class_name)?;
        Self::new_unix_domain(None, &unix_dom, initial, ui, no_auto_start, Weak::new())
    }

    pub fn new_unix_domain(
        local_domain_id: Option<DomainId>,
        unix_dom: &UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_dom.clone()), None);
        reconnectable.connect(initial, ui, no_auto_start)?;
        Ok(Self::new(local_domain_id, reconnectable, mux_owner))
    }

    pub fn new_tls(
        local_domain_id: DomainId,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Tls(tls_client.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable, mux_owner))
    }

    pub fn new_ssh(
        local_domain_id: DomainId,
        ssh_dom: &SshDomain,
        ui: &mut ConnectionUI,
        mux_owner: Weak<Mux>,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Ssh(ssh_dom.clone()), None);
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        Ok(Self::new(Some(local_domain_id), reconnectable, mux_owner))
    }

    pub fn send_pdu(
        &self,
        pdu: Pdu,
    ) -> impl std::future::Future<Output = anyhow::Result<Pdu>> + Send + 'static {
        self.rpc_scope().send_pdu(pdu)
    }

    fn send_pdu_expect(
        &self,
        pdu: Pdu,
        expected_response_ident: Option<NonZeroU64>,
    ) -> impl std::future::Future<Output = anyhow::Result<Pdu>> + Send + 'static {
        self.rpc_scope()
            .send_pdu_expect(pdu, expected_response_ident)
    }

    pub(crate) fn rpc_scope(&self) -> RpcGenerationScope {
        RpcGenerationScope::capture(self.sender.clone(), Arc::clone(&self.rpc_transport))
    }

    #[allow(dead_code)]
    pub(crate) fn agreed_codec_version(&self) -> Option<usize> {
        self.rpc_scope()
            .codec_authority()
            .map(|authority| authority.agreed)
    }

    pub(crate) fn bootstrap_rpc_scope(&self) -> RpcGenerationScope {
        RpcGenerationScope::bootstrap(self.sender.clone(), Arc::clone(&self.rpc_transport))
    }

    fn bootstrap_rpc_scope_at(&self, generation: NonZeroU64) -> RpcGenerationScope {
        RpcGenerationScope::exact(
            self.sender.clone(),
            Arc::clone(&self.rpc_transport),
            generation,
            true,
        )
    }

    pub(crate) async fn publish_rpc_transport_ready(
        &self,
        rpc: &RpcGenerationScope,
        readiness_guard: &RpcGenerationAbortGuard,
    ) -> anyhow::Result<()> {
        if !Arc::ptr_eq(&self.rpc_transport, &rpc.rpc_transport) {
            bail!("cannot publish mux RPC readiness from a foreign client scope");
        }
        let generation = rpc
            .generation
            .ok_or_else(|| anyhow!("cannot publish readiness for an unavailable RPC scope"))?;
        let reader_abort = rpc
            .reader_abort
            .as_ref()
            .filter(|authority| authority.generation == generation)
            .cloned()
            .ok_or_else(|| anyhow!("readiness publisher lacks exact reader authority"))?;
        let readiness_authority = {
            let lifecycle = self.rpc_transport.lifecycle.lock();
            if !matches!(
                lifecycle.phase,
                RpcTransportPhase::Live(observed) if observed == generation
            ) || self
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire)
                != generation.get()
            {
                bail!(
                    "cannot publish readiness for retired mux RPC generation {}",
                    generation
                );
            }
            if lifecycle.readiness_authority.generation != generation {
                bail!(
                    "mux RPC readiness authority generation {} does not match publisher {}",
                    lifecycle.readiness_authority.generation,
                    generation
                );
            }
            let protocol = lifecycle
                .protocol_for(generation)
                .map_err(anyhow::Error::new)?;
            if protocol.phase != RpcProtocolPhase::Established || protocol.codec.is_none() {
                bail!(
                    "cannot publish mux RPC generation {} readiness before codec negotiation and \
                     client registration are established (phase {:?})",
                    generation,
                    protocol.phase
                );
            }
            if self
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire)
                == generation.get()
            {
                return Ok(());
            }
            if !Arc::ptr_eq(&lifecycle.reader_abort, &reader_abort) {
                bail!(
                    "mux RPC generation {} readiness publisher has stale reader authority",
                    generation
                );
            }
            if !readiness_guard.authorizes_pending_readiness(
                &self.rpc_transport,
                generation,
                &lifecycle.readiness_authority,
            ) {
                bail!(
                    "mux RPC generation {} readiness publication lacks its live participant guard",
                    generation
                );
            }
            Arc::clone(&lifecycle.readiness_authority)
        };
        let reservation = readiness_authority.reserve_publication()?;
        let (promise, result) = bounded(1);
        self.sender
            .try_send(ReaderMessage::PublishReady {
                generation,
                reader_sender: self.sender.clone(),
                promise,
                reservation,
            })
            .map_err(|_| anyhow!("mux RPC reader queue closed before readiness publication"))?;
        result
            .recv()
            .await
            .map_err(|_| anyhow!("mux RPC reader dropped readiness publication"))??;
        self.rpc_transport.validate_live_control_ack(
            generation,
            &reader_abort,
            "readiness publication",
        )
    }

    pub(crate) fn abort_rpc_transport_generation(
        &self,
        rpc: &RpcGenerationScope,
        reason: &'static str,
    ) -> anyhow::Result<()> {
        if !Arc::ptr_eq(&self.rpc_transport, &rpc.rpc_transport) {
            bail!("cannot abort a mux RPC generation from a foreign client scope");
        }
        let generation = rpc
            .generation
            .ok_or_else(|| anyhow!("cannot abort an unavailable mux RPC scope"))?;
        let reader_abort = rpc
            .reader_abort
            .as_ref()
            .filter(|authority| authority.generation == generation)
            .ok_or_else(|| anyhow!("mux RPC scope has no exact reader abort authority"))?;
        if self
            .rpc_transport
            .request_generation_abort(reader_abort, reason)
            || reader_abort.aborted_error().is_some()
        {
            Ok(())
        } else {
            bail!(
                "mux RPC generation {} could not be fenced because its exact reader authority is stale",
                generation
            )
        }
    }

    pub async fn resolve_pane_id(&self, pane_id: Option<PaneId>) -> anyhow::Result<PaneId> {
        let pane_id: PaneId = match pane_id {
            Some(p) => p,
            None => {
                if let Ok(pane) = std::env::var("WEZTERM_PANE") {
                    pane.parse()?
                } else {
                    let mut clients = self.list_clients().await?.clients;
                    clients.retain(|client| client.focused_pane_id.is_some());
                    clients.sort_by_key(|b| std::cmp::Reverse(b.last_input));
                    if clients.is_empty() {
                        anyhow::bail!(
                            "--pane-id was not specified and $WEZTERM_PANE
                         is not set in the environment, and I couldn't
                         determine which pane was currently focused"
                        );
                    }

                    clients[0]
                        .focused_pane_id
                        .expect("to have filtered out above")
                }
            }
        };
        Ok(pane_id)
    }

    rpc_surface!();
}

// Exact-generation scopes intentionally mirror the complete client RPC surface.
// Individual flows use coherent subsets, but keeping one generated surface
// prevents a future call site from escaping back to ambient generation lookup.
#[allow(dead_code)]
impl RpcGenerationScope {
    rpc_surface!();
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestRpcPeer {
    receiver: Receiver<ReaderMessage>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TestReliableWireRequest {
    pub(crate) traced: bool,
    pub(crate) request: ReliableKeyEventV1,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TestReliablePaneWriteWireRequest {
    pub(crate) request: ReliablePaneWriteV1,
}

#[cfg(test)]
impl TestRpcPeer {
    pub(crate) fn activate_reconnect_generation(
        &self,
        client: &Client,
    ) -> anyhow::Result<NonZeroU64> {
        if !self.receiver.is_empty() {
            bail!("test reconnect activation requires an empty outbound queue");
        }
        let current = client.test_dispatch_authority(Weak::new());
        let successor = current.advance_generation(&self.receiver)?;
        successor.activate_rpc_transport()?;
        client
            .rpc_transport
            .active_generation()
            .ok_or_else(|| anyhow!("test reconnect generation did not become live"))
    }

    pub(crate) fn replace_ready_generation(
        &self,
        client: &Client,
        agreed_codec_version: usize,
    ) -> anyhow::Result<NonZeroU64> {
        if !self.receiver.is_empty() {
            bail!("test codec-generation barrier requires an empty outbound queue");
        }
        let current = client.test_dispatch_authority(Weak::new());
        let successor = current.advance_generation(&self.receiver)?;
        successor.activate_rpc_transport()?;
        client
            .rpc_transport
            .mark_current_generation_ready_with_codec_for_test(agreed_codec_version);
        let generation = client
            .rpc_transport
            .active_generation()
            .ok_or_else(|| anyhow!("test successor generation did not become live"))?;
        client
            .rpc_transport
            .bind_render_connection_identity(generation, TEST_RENDER_CONNECTION_IDENTITY)?;
        Ok(generation)
    }

    pub(crate) async fn respond_next_reliable_applied(
        &self,
    ) -> anyhow::Result<TestReliableWireRequest> {
        let message = self
            .receiver
            .recv()
            .await
            .context("test RPC peer queue closed before reliable input")?;
        let ReaderMessage::SendPdu {
            binding,
            lease,
            promise,
        } = message
        else {
            bail!("test RPC peer received a non-PDU control message");
        };
        if !lease.matches(binding) {
            bail!("test reliable RPC lease lost its exact binding");
        }
        let prepared = lease
            .claim_for_reader()?
            .ok_or_else(|| anyhow!("test reliable RPC was cancelled before reader claim"))?;
        let wire_request = match prepared.pdu() {
            Pdu::ReliableKeyEventV1(request) => TestReliableWireRequest {
                traced: false,
                request: request.clone(),
            },
            Pdu::ReliableKeyEventTracedV1(request) => TestReliableWireRequest {
                traced: true,
                request: request.request.clone(),
            },
            other => bail!(
                "test reliable RPC peer received unexpected {}",
                other.pdu_name()
            ),
        };
        let response = Pdu::ReliableKeyEventV1Response(ReliableKeyEventV1Response {
            pane_id: wire_request.request.pane_id,
            input_serial: wire_request.request.input_serial,
            outcome: ReliableKeyEventOutcomeV1::Applied,
        });
        promise
            .send(Ok(PendingRpcReply::pdu(response)))
            .await
            .map_err(|_| anyhow!("test reliable RPC caller retired before response"))?;
        drop(prepared);
        Ok(wire_request)
    }

    pub(crate) async fn respond_next_reliable_pane_write(
        &self,
        outcome: ReliablePaneWriteOutcomeV1,
    ) -> anyhow::Result<TestReliablePaneWriteWireRequest> {
        let message = self
            .receiver
            .recv()
            .await
            .context("test RPC peer queue closed before reliable pane write")?;
        let ReaderMessage::SendPdu {
            binding,
            lease,
            promise,
        } = message
        else {
            bail!("test RPC peer received a non-PDU control message");
        };
        if !lease.matches(binding) {
            bail!("test reliable pane-write RPC lease lost its exact binding");
        }
        let prepared = lease.claim_for_reader()?.ok_or_else(|| {
            anyhow!("test reliable pane-write RPC was cancelled before reader claim")
        })?;
        let request = match prepared.pdu() {
            Pdu::ReliablePaneWriteV1(request) => request.clone(),
            other => bail!(
                "test reliable pane-write RPC peer received unexpected {}",
                other.pdu_name()
            ),
        };
        let response = Pdu::ReliablePaneWriteV1Response(ReliablePaneWriteV1Response {
            pane_id: request.pane_id,
            input_serial: request.input_serial,
            outcome,
        });
        promise
            .send(Ok(PendingRpcReply::pdu(response)))
            .await
            .map_err(|_| anyhow!("test reliable pane-write RPC caller retired before response"))?;
        drop(prepared);
        Ok(TestReliablePaneWriteWireRequest { request })
    }

    pub(crate) async fn respond_next_unit(&self) -> anyhow::Result<Pdu> {
        let message = self
            .receiver
            .recv()
            .await
            .context("test RPC peer queue closed before unit-response request")?;
        let ReaderMessage::SendPdu {
            binding,
            lease,
            promise,
        } = message
        else {
            bail!("test RPC peer received a non-PDU control message");
        };
        if !lease.matches(binding) {
            bail!("test unit-response RPC lease lost its exact binding");
        }
        let prepared = lease
            .claim_for_reader()?
            .ok_or_else(|| anyhow!("test unit-response RPC was cancelled before reader claim"))?;
        let request = match prepared.pdu() {
            Pdu::WriteToPane(request) => Pdu::WriteToPane(request.clone()),
            Pdu::SendPaste(request) => Pdu::SendPaste(request.clone()),
            Pdu::SendPasteTracedV1(request) => Pdu::SendPasteTracedV1(request.clone()),
            other => bail!(
                "test unit-response RPC peer received unexpected {}",
                other.pdu_name()
            ),
        };
        promise
            .send(Ok(PendingRpcReply::pdu(Pdu::UnitResponse(UnitResponse {}))))
            .await
            .map_err(|_| anyhow!("test unit-response RPC caller retired before response"))?;
        drop(prepared);
        Ok(request)
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

#[cfg(test)]
impl Client {
    fn test_dispatch_authority(&self, mux_owner: Weak<Mux>) -> ClientDispatchAuthority {
        ClientDispatchAuthority::new(
            self.local_domain_id,
            mux_owner,
            Arc::clone(&self.incarnation),
            Arc::clone(&self.connection_generation),
            Arc::clone(&self.rpc_transport),
        )
    }

    fn test_reader_message(
        &self,
        pdu: Pdu,
        promise: Sender<anyhow::Result<PendingRpcReply>>,
    ) -> ReaderMessage {
        let request = pdu.pdu_name();
        let prepared = pdu
            .prepare_outbound_for_dialect(
                MuxWireDialect::current(CODEC_VERSION)
                    .expect("tests use the current exact wire dialect"),
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("test request should produce an owned outbound plan");
        let attempt_id = self
            .rpc_transport
            .allocate_attempt(request)
            .expect("test RPC attempt identity should be available");
        let generation = self
            .rpc_transport
            .active_generation()
            .expect("test RPC transport should be live");
        let lease = self
            .rpc_transport
            .outbound_budget
            .try_reserve(Arc::downgrade(&self.rpc_transport), generation, prepared)
            .expect("test request should fit the client outbound budget");
        ReaderMessage::SendPdu {
            binding: RpcBinding {
                generation,
                attempt_id,
                request,
                expected_response_ident: None,
            },
            lease,
            promise,
        }
    }

    pub(crate) fn new_test_client(
        local_domain_id: Option<DomainId>,
        client_domain_config: ClientDomainConfig,
    ) -> Self {
        let (sender, _receiver) = unbounded();
        let rpc_transport = Arc::new(RpcTransportState::new());
        rpc_transport.mark_current_generation_ready_for_test();
        rpc_transport
            .bind_render_connection_identity(
                NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
                    .expect("initial test generation is nonzero"),
                TEST_RENDER_CONNECTION_IDENTITY,
            )
            .expect("test client should bind a valid render connection identity");
        Self {
            sender,
            local_domain_id,
            // Mirror the production constructor: a client with no local domain
            // starts authorized, one bound to a local domain does not.
            domain_reconnect_authorized: Arc::new(AtomicBool::new(local_domain_id.is_none())),
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport,
            client_id: ClientId {
                hostname: "test-host".to_string(),
                username: "tester".to_string(),
                pid: 1,
                epoch: 1,
                id: 1,
                ssh_auth_sock: None,
            },
            client_domain_config,
            is_reconnectable: false,
            is_local: true,
        }
    }

    pub(crate) fn new_test_client_with_rpc_peer(
        local_domain_id: Option<DomainId>,
        client_domain_config: ClientDomainConfig,
    ) -> (Self, TestRpcPeer) {
        let (sender, receiver) = unbounded();
        let rpc_transport = Arc::new(RpcTransportState::new());
        rpc_transport.mark_current_generation_ready_for_test();
        rpc_transport
            .bind_render_connection_identity(
                NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
                    .expect("initial test generation is nonzero"),
                TEST_RENDER_CONNECTION_IDENTITY,
            )
            .expect("test client should bind a valid render connection identity");
        (
            Self {
                sender,
                local_domain_id,
                // Mirror the production constructor: a client with no local
                // domain starts authorized, one bound to a local domain does not.
                domain_reconnect_authorized: Arc::new(AtomicBool::new(local_domain_id.is_none())),
                incarnation: Arc::new(ClientIncarnation),
                connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
                rpc_transport,
                client_id: ClientId {
                    hostname: "test-host".to_string(),
                    username: "tester".to_string(),
                    pid: 1,
                    epoch: 1,
                    id: 1,
                    ssh_auth_sock: None,
                },
                client_domain_config,
                is_reconnectable: false,
                is_local: true,
            },
            TestRpcPeer { receiver },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MuxTestScope;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::time::VirtualClock;
    use asupersync::types::Time;
    use codec::{
        GetCodecVersionResponse, PaneRemoved, SetClientId, UnitResponse, WindowTitleChanged,
        WindowWorkspaceChanged,
    };
    use metrics::atomics::AtomicU64 as MetricAtomicU64;
    use metrics::{Counter, Gauge};
    use mux::tab::{PaneEntry, PaneNode};
    use std::fmt;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::sync::{Mutex as StdMutex, Once};
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};
    use wezterm_term::TerminalSize;

    static TEST_LOGGER: TestLogger = TestLogger {
        records: StdMutex::new(Vec::new()),
    };
    static TEST_LOGGER_INIT: Once = Once::new();
    const MAX_CAPTURED_COMPAT_WARNINGS: usize = 32;
    #[cfg(unix)]
    static TEST_SOCKET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestLogger {
        records: StdMutex<Vec<String>>,
    }

    struct CountingPollWake {
        count: Arc<AtomicU64>,
    }

    impl futures::task::ArcWake for CountingPollWake {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.count.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    struct BlockingDropPollWake {
        drop_started: Arc<std::sync::Barrier>,
        allow_drop: Arc<std::sync::Barrier>,
    }

    impl futures::task::ArcWake for BlockingDropPollWake {
        fn wake_by_ref(_arc_self: &Arc<Self>) {}
    }

    impl Drop for BlockingDropPollWake {
        fn drop(&mut self) {
            self.drop_started.wait();
            self.allow_drop.wait();
        }
    }

    fn counting_poll_waker(count: &Arc<AtomicU64>) -> std::task::Waker {
        futures::task::waker(Arc::new(CountingPollWake {
            count: Arc::clone(count),
        }))
    }

    fn test_poll_cadence(
        slot_spacing_micros: u64,
    ) -> (
        Arc<BoundedPollCadence>,
        Arc<AtomicU64>,
        Arc<VirtualClock>,
        TimerDriverHandle,
    ) {
        let now_micros = Arc::new(AtomicU64::new(0));
        let cadence_now = Arc::clone(&now_micros);
        let cadence = BoundedPollCadence::with_now(slot_spacing_micros, move || {
            cadence_now.load(AtomicOrdering::Acquire)
        });
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
        (cadence, now_micros, clock, timer)
    }

    fn advance_poll_cadence(
        now_micros: &AtomicU64,
        clock: &VirtualClock,
        timer: &TimerDriverHandle,
        target_micros: u64,
    ) -> usize {
        now_micros.store(target_micros, AtomicOrdering::Release);
        clock.set(Time::from_nanos(target_micros.saturating_mul(1_000)));
        timer.process_timers()
    }

    #[test]
    fn zero_reconnect_attempt_limit_never_exhausts_either_retry_budget() {
        for observed_attempts in [0, 1, 2, u32::MAX] {
            assert!(!reconnect_cycle_budget_exhausted(0, observed_attempts));
            assert!(!reconnect_dial_budget_exhausted(0, observed_attempts));
        }

        assert!(!reconnect_cycle_budget_exhausted(3, 3));
        assert!(reconnect_cycle_budget_exhausted(3, 4));
        assert!(!reconnect_dial_budget_exhausted(3, 2));
        assert!(reconnect_dial_budget_exhausted(3, 3));
    }

    #[test]
    fn reconnect_backoff_saturates_before_applying_the_configured_ceiling() {
        assert_eq!(
            next_reconnect_backoff(Duration::from_secs(3), Duration::from_secs(10)),
            Duration::from_secs(6)
        );
        assert_eq!(
            next_reconnect_backoff(Duration::from_secs(8), Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_reconnect_backoff(Duration::MAX, Duration::MAX),
            Duration::MAX
        );
    }

    #[test]
    fn reconnect_backoff_cannot_be_bypassed_by_a_closed_ui() {
        let delay = Duration::from_millis(20);
        let started = std::time::Instant::now();
        let error = wait_for_reconnect_backoff(delay, |_| {
            anyhow::bail!("planted disconnected connection UI")
        })
        .expect("the planted UI failure is retained for diagnostics");

        assert!(error.to_string().contains("disconnected connection UI"));
        assert!(
            started.elapsed() >= delay,
            "a failed UI timer must not accelerate the reconnect dial cadence"
        );
    }

    #[test]
    fn reconnect_backoff_does_not_trust_an_early_successful_ui_reply() {
        let delay = Duration::from_millis(20);
        let started = std::time::Instant::now();
        assert!(wait_for_reconnect_backoff(delay, |_| Ok(())).is_none());
        assert!(
            started.elapsed() >= delay,
            "the reconnect worker owns the minimum delay even if a UI replies early"
        );
    }

    #[test]
    fn windows_ssh_poll_backoff_is_exponential_and_bounded() {
        let backoff = AtomicU8::new(0);
        let observed = (0..8)
            .map(|_| next_bounded_poll_backoff(&backoff))
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                Duration::from_millis(4),
                Duration::from_millis(8),
                Duration::from_millis(16),
                Duration::from_millis(32),
                Duration::from_millis(64),
                Duration::from_millis(128),
                Duration::from_millis(250),
                Duration::from_millis(250),
            ]
        );
    }

    #[test]
    fn windows_ssh_global_poll_cadence_caps_fleet_wake_rate() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let wakes = Arc::new(AtomicU64::new(0));
        let reservations = (0..1_001)
            .map(|_| cadence.reserve(timer.clone(), counting_poll_waker(&wakes), Duration::ZERO))
            .collect::<Vec<_>>();

        for slot in 0..500 {
            assert_eq!(
                advance_poll_cadence(&now_micros, &clock, &timer, slot * 2_000),
                1,
                "each cadence slot must use exactly one dispatcher timer"
            );
        }
        assert_eq!(
            wakes.load(AtomicOrdering::SeqCst),
            500,
            "the shared cadence must wake at most 500 fallback polls before the one-second boundary"
        );
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 999_999),
            0,
            "the dispatcher must not create a boundary burst"
        );
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 1_000_000),
            1
        );
        assert_eq!(wakes.load(AtomicOrdering::SeqCst), 501);
        assert_eq!(cadence.pending_reservations(), 500);
        drop(reservations);
        assert_eq!(cadence.pending_reservations(), 0);
    }

    #[test]
    fn windows_ssh_spurious_timer_wake_cannot_bypass_the_effective_cadence_deadline() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let first_wakes = Arc::new(AtomicU64::new(0));
        let second_wakes = Arc::new(AtomicU64::new(0));
        let first = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&first_wakes),
            Duration::ZERO,
        );
        let second = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&second_wakes),
            Duration::ZERO,
        );

        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(first_wakes.load(AtomicOrdering::SeqCst), 1);
        let early_timer_live = cadence
            .active_timer_live_for_test()
            .expect("the second wake must own the next cadence timer");

        // The second reservation's own not-before time is zero, but its
        // effective global-cadence deadline is 2 ms. Plant a spurious wake at
        // 1 ms; checking only the reservation deadline would dispatch early.
        now_micros.store(1_000, AtomicOrdering::Release);
        clock.set(Time::from_nanos(1_000_000));
        early_timer_live.store(false, AtomicOrdering::Release);
        cadence.timer_fired(&early_timer_live);
        assert_eq!(second_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(cadence.pending_reservations(), 1);
        assert_eq!(timer.pending_count(), 1);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 1_999), 0);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 2_000), 1);
        assert_eq!(second_wakes.load(AtomicOrdering::SeqCst), 1);
        drop((first, second));
    }

    #[test]
    fn windows_ssh_poll_cadence_reclaims_cancelled_middle_and_tail() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let first_wakes = Arc::new(AtomicU64::new(0));
        let cancelled_middle_wakes = Arc::new(AtomicU64::new(0));
        let surviving_wakes = Arc::new(AtomicU64::new(0));
        let cancelled_tail_wakes = Arc::new(AtomicU64::new(0));
        let first = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&first_wakes),
            Duration::ZERO,
        );
        let cancelled_middle = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&cancelled_middle_wakes),
            Duration::ZERO,
        );
        let surviving = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&surviving_wakes),
            Duration::ZERO,
        );
        let cancelled_tail = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&cancelled_tail_wakes),
            Duration::ZERO,
        );

        drop(cancelled_middle);
        drop(cancelled_tail);
        assert_eq!(cadence.pending_reservations(), 2);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(first_wakes.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 2_000), 1);
        assert_eq!(surviving_wakes.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(cancelled_middle_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(cancelled_tail_wakes.load(AtomicOrdering::SeqCst), 0);
        drop((first, surviving));
    }

    #[test]
    fn windows_ssh_cancel_winning_dispatch_race_does_not_consume_a_slot() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let retired_wakes = Arc::new(AtomicU64::new(0));
        let live_wakes = Arc::new(AtomicU64::new(0));
        let retired = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&retired_wakes),
            Duration::ZERO,
        );
        let live = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&live_wakes),
            Duration::ZERO,
        );

        // Plant the exact race where cancellation retires the reservation
        // before its queue-unlink path acquires the cadence mutex.
        retired
            .reservation_live
            .store(false, AtomicOrdering::Release);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(retired_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 0),
            1,
            "a cancelled wake must not spend the next live stream's cadence slot"
        );
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 1);
        drop((retired, live));
    }

    #[test]
    fn windows_ssh_fired_handle_cannot_cancel_a_reused_reservation_id() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let retired_wakes = Arc::new(AtomicU64::new(0));
        let retired = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&retired_wakes),
            Duration::ZERO,
        );
        let retired_id = retired.reservation_id;
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(retired_wakes.load(AtomicOrdering::SeqCst), 1);

        // Force the allocator's wrap/collision seam without 2^64 inserts.
        // The fired handle still owns its old numeric ID, but no cancellation
        // authority; dropping it must not unlink the new live reservation.
        cadence.state.lock().next_reservation_id = retired_id;
        let live_wakes = Arc::new(AtomicU64::new(0));
        let live = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&live_wakes),
            Duration::ZERO,
        );
        assert_eq!(live.reservation_id, retired_id);
        drop(retired);
        assert_eq!(cadence.pending_reservations(), 1);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 2_000), 1);
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 1);
        drop(live);
    }

    #[test]
    fn windows_ssh_cancelled_churn_cannot_create_future_poll_debt() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let churn_wakes = Arc::new(AtomicU64::new(0));
        let fallback = SshPollFallback::new();
        for _ in 0..20_000 {
            fallback.arm(
                &cadence,
                timer.clone(),
                counting_poll_waker(&churn_wakes),
                Duration::ZERO,
            );
        }
        assert_eq!(
            cadence.pending_reservations(),
            1,
            "one stream may own only one fallback reservation"
        );
        fallback.reset();
        assert_eq!(cadence.pending_reservations(), 0);
        assert_eq!(timer.pending_count(), 0);

        let live_wakes = Arc::new(AtomicU64::new(0));
        let live = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&live_wakes),
            Duration::ZERO,
        );
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(churn_wakes.load(AtomicOrdering::SeqCst), 0);
        drop(live);
    }

    #[test]
    fn windows_ssh_reset_during_arm_retirement_cannot_resurrect_a_stale_wake() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let fallback = Arc::new(SshPollFallback::new());
        let drop_started = Arc::new(std::sync::Barrier::new(2));
        let allow_drop = Arc::new(std::sync::Barrier::new(2));
        {
            let mut state = fallback.lock_reservation();
            state.generation = Some(u64::MAX - 2);
        }
        fallback.arm(
            &cadence,
            timer.clone(),
            futures::task::waker(Arc::new(BlockingDropPollWake {
                drop_started: Arc::clone(&drop_started),
                allow_drop: Arc::clone(&allow_drop),
            })),
            Duration::from_millis(250),
        );
        let replacement_wakes = Arc::new(AtomicU64::new(0));

        std::thread::scope(|scope| {
            scope.spawn(|| {
                fallback.arm(
                    &cadence,
                    timer.clone(),
                    counting_poll_waker(&replacement_wakes),
                    Duration::ZERO,
                );
            });

            // The replacement arm has incremented its generation and removed
            // the old reservation, but is paused while that reservation drops
            // its foreign waker outside the stream mutex. Reset must retire
            // the in-flight generation before it can publish a new wake.
            drop_started.wait();
            let (reset_done_tx, reset_done_rx) = std::sync::mpsc::sync_channel(1);
            // `reset_done_tx` is created inside the scope body, so it cannot be
            // borrowed by a scoped thread that must outlive that body; move it
            // in. `fallback` is still used after the scope, so clone the Arc
            // rather than moving the original.
            let fallback_for_reset = Arc::clone(&fallback);
            scope.spawn(move || {
                fallback_for_reset.reset();
                let _ = reset_done_tx.send(());
            });
            let reset_completed_while_drop_was_blocked =
                reset_done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
            allow_drop.wait();
            assert!(
                reset_completed_while_drop_was_blocked,
                "reset must not wait for a foreign waker destructor"
            );
        });

        assert_eq!(cadence.pending_reservations(), 0);
        assert_eq!(timer.pending_count(), 0);
        assert_eq!(fallback.lock_reservation().generation, None);
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 250_000),
            0
        );
        assert_eq!(replacement_wakes.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn windows_ssh_poll_generation_exhaustion_is_terminal_without_aba_wrap() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let fallback = SshPollFallback::new();
        {
            let mut state = fallback.lock_reservation();
            state.generation = Some(u64::MAX - 1);
        }
        let last_generation_wakes = Arc::new(AtomicU64::new(0));
        assert!(fallback.arm(
            &cadence,
            timer.clone(),
            counting_poll_waker(&last_generation_wakes),
            Duration::from_millis(250),
        ));
        assert_eq!(fallback.lock_reservation().generation, Some(u64::MAX));
        assert_eq!(cadence.pending_reservations(), 1);

        let wrapped_generation_wakes = Arc::new(AtomicU64::new(0));
        assert!(!fallback.arm(
            &cadence,
            timer.clone(),
            counting_poll_waker(&wrapped_generation_wakes),
            Duration::ZERO,
        ));
        assert_eq!(fallback.lock_reservation().generation, None);
        assert_eq!(cadence.pending_reservations(), 0);
        assert_eq!(timer.pending_count(), 0);
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 250_000),
            0
        );
        assert_eq!(last_generation_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(wrapped_generation_wakes.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn windows_ssh_poll_fallback_drop_cancels_timer_and_rejects_stale_wake() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let retired_wakes = Arc::new(AtomicU64::new(0));
        let retired = SshPollFallback::new();
        retired.arm(
            &cadence,
            timer.clone(),
            counting_poll_waker(&retired_wakes),
            Duration::from_millis(250),
        );
        let retired_timer_live = cadence
            .active_timer_live_for_test()
            .expect("the retired fallback must initially own the dispatcher timer");
        assert_eq!(timer.pending_count(), 1);

        drop(retired);
        assert_eq!(cadence.pending_reservations(), 0);
        assert_eq!(timer.pending_count(), 0);

        let live_wakes = Arc::new(AtomicU64::new(0));
        let live = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&live_wakes),
            Duration::from_millis(10),
        );
        cadence.timer_fired(&retired_timer_live);
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(cadence.pending_reservations(), 1);
        assert_eq!(timer.pending_count(), 1);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 9_999), 0);
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 10_000), 1);
        assert_eq!(live_wakes.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(retired_wakes.load(AtomicOrdering::SeqCst), 0);
        drop(live);
    }

    #[test]
    fn windows_ssh_poll_cadence_is_fair_to_unrelated_live_streams() {
        let (cadence, now_micros, clock, timer) = test_poll_cadence(2_000);
        let churn_wakes = Arc::new(AtomicU64::new(0));
        let unrelated_wakes = Arc::new(AtomicU64::new(0));
        let first_churn = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&churn_wakes),
            Duration::ZERO,
        );
        let unrelated = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&unrelated_wakes),
            Duration::ZERO,
        );

        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 0), 1);
        assert_eq!(churn_wakes.load(AtomicOrdering::SeqCst), 1);
        let churn_successor = cadence.reserve(
            timer.clone(),
            counting_poll_waker(&churn_wakes),
            Duration::ZERO,
        );
        assert_eq!(
            advance_poll_cadence(&now_micros, &clock, &timer, 0),
            0,
            "a self-rearming stream must not create a same-boundary burst"
        );
        assert_eq!(advance_poll_cadence(&now_micros, &clock, &timer, 2_000), 1);
        assert_eq!(
            unrelated_wakes.load(AtomicOrdering::SeqCst),
            1,
            "the already-waiting unrelated stream must precede the churn successor"
        );
        assert_eq!(churn_wakes.load(AtomicOrdering::SeqCst), 1);
        drop((first_churn, unrelated, churn_successor));
    }

    #[test]
    fn reconnect_dispatch_source_borrows_domain_guard_through_post_await_check() {
        let source = include_str!("client.rs");
        let dispatch_start =
            ["match reconnect_dispatch_authority.", "resolve_current() {"].concat();
        let none_branch = ["Ok(None)", " => {"].concat();
        let start = source
            .find(&dispatch_start)
            .expect("production reconnect dispatch match must remain present");
        let end = source[start..]
            .find(&none_branch)
            .map(|offset| start + offset + none_branch.len())
            .expect("production reconnect dispatch match must retain its Ok(None) branch");
        let reconnect_dispatch = &source[start..end];
        let raw_domain_clone = ["Arc::clone(&dispatch.", "domain)"].concat();
        let borrowed_domain = ["&dispatch.", "domain,"].concat();
        assert!(
            !reconnect_dispatch.contains(&raw_domain_clone),
            "reconnect dispatch must never clone a raw Domain Arc out of its exact guard"
        );
        assert_eq!(
            reconnect_dispatch.matches(&borrowed_domain).count(),
            1,
            "the reconnect call must borrow exactly one dispatch-owned domain guard"
        );

        let reconnect = reconnect_dispatch
            .find("ClientDomain::reattach_if_current(")
            .expect("production reconnect dispatch call must remain present");
        let result_assignment = reconnect_dispatch[..reconnect]
            .rfind("let result")
            .expect("production reconnect dispatch must retain the result binding");
        assert_eq!(
            reconnect_dispatch[result_assignment..reconnect].trim_end(),
            "let result =",
            "the awaited reattach result must remain bound for failure reporting"
        );
        let await_end = reconnect_dispatch[reconnect..]
            .find(".await;")
            .map(|offset| reconnect + offset + ".await;".len())
            .expect("production reconnect call must remain awaited");
        let post_check = ["if !dispatch.", "rpc_generation_is_live()"].concat();
        let post_check_position = reconnect_dispatch[await_end..]
            .find(&post_check)
            .map(|offset| await_end + offset)
            .expect("production reconnect dispatch must retain its liveness check");
        let failure_branch = reconnect_dispatch[await_end..]
            .find("if let Err(err) = result")
            .map(|offset| await_end + offset)
            .expect("production reconnect dispatch must report reattach failure");
        let failure_report = reconnect_dispatch[failure_branch..]
            .find("remains unavailable because codec/topology reattach failed")
            .map(|offset| failure_branch + offset)
            .expect("reattach failure must be surfaced to the reconnect UI");
        let success_report = reconnect_dispatch[await_end..]
            .find("Reconnected and reattached domain")
            .map(|offset| await_end + offset)
            .expect("reattach success must be surfaced only after verification");
        assert!(
            post_check_position > await_end,
            "dispatch and its domain guard must stay alive through the post-await liveness check"
        );
        assert!(
            failure_branch < post_check_position && failure_report < post_check_position,
            "a failed codec/topology reattach retires its generation, so its error must be reported before the success-only liveness guard"
        );
        assert!(
            success_report > post_check_position,
            "the reconnect UI must not claim success before reattach and generation verification"
        );
        assert!(
            !reconnect_dispatch[..reconnect].contains("Reconnected!"),
            "transport establishment alone must never be reported as a recovered domain"
        );
        assert!(
            !source.contains("ui.output_str(\"Connected!\\n\")")
                && !source.contains("ui.output_str(\"TLS Connected!\\n\")"),
            "transport setup must describe protocol verification as pending"
        );
        let scheduler_rejection = reconnect_dispatch
            .find("cannot schedule reconnect reattach")
            .expect("reconnect dispatch must retain bounded scheduler rejection handling");
        let scheduler_retry = &reconnect_dispatch[scheduler_rejection..];
        assert!(
            scheduler_retry.contains("abort_rpc_transport_generation(")
                && scheduler_retry.contains("successor topology scheduler admission failed")
                && scheduler_retry.contains("reconnected = true"),
            "transient scheduler rejection must fence the exact successor and enter ordinary generation retirement before retry"
        );
    }

    impl log::Log for TestLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() == log::Level::Warn
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let message = record.args().to_string();
            if !message.contains("Codec compat window:") {
                return;
            }
            let mut records = self.records.lock().expect("test logger lock");
            if records.len() < MAX_CAPTURED_COMPAT_WARNINGS {
                records.push(format!("{} {message}", record.level()));
            }
        }

        fn flush(&self) {}
    }

    fn reset_test_logger() {
        TEST_LOGGER_INIT.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("install test logger");
            log::set_max_level(log::LevelFilter::Warn);
        });
        TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock")
            .clear();
    }

    fn captured_logs() -> Vec<String> {
        TEST_LOGGER
            .records
            .lock()
            .expect("test logger lock")
            .clone()
    }

    #[test]
    fn compatibility_warning_capture_is_exact_and_bounded() {
        let logger = TestLogger {
            records: StdMutex::new(Vec::new()),
        };
        let info = log::Record::builder()
            .level(log::Level::Info)
            .args(format_args!("Codec compat window: server=info"))
            .build();
        log::Log::log(&logger, &info);
        let unrelated_warning = log::Record::builder()
            .level(log::Level::Warn)
            .args(format_args!("unrelated warning"))
            .build();
        log::Log::log(&logger, &unrelated_warning);

        for index in 0..=MAX_CAPTURED_COMPAT_WARNINGS {
            log::Log::log(
                &logger,
                &log::Record::builder()
                    .level(log::Level::Warn)
                    .args(format_args!("Codec compat window: server={}", index))
                    .build(),
            );
        }

        let records = logger.records.lock().expect("test logger lock");
        assert_eq!(records.len(), MAX_CAPTURED_COMPAT_WARNINGS);
        assert!(records.iter().all(|record| record.starts_with("WARN ")));
        assert!(records
            .last()
            .is_some_and(|record| record.ends_with("server=31")));
    }

    fn asupersync_block_on<F: std::future::Future>(future: F) -> F::Output {
        RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build frankenterm-client asupersync runtime")
            .block_on(future)
    }

    fn assert_expected_reader_shutdown(result: anyhow::Result<()>) {
        let error = result.expect_err("reader must not report success when its transport ends");
        let client_destroyed = error
            .downcast_ref::<NotReconnectableError>()
            .is_some_and(|kind| *kind == NotReconnectableError::ClientWasDestroyed);
        let clean_eof = error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == ErrorKind::UnexpectedEof);
        assert!(
            client_destroyed || clean_eof,
            "unexpected reader shutdown disposition: {:#}",
            error
        );
    }

    /// Cancellable hang watchdog. Returns a guard; dropping it (test finished,
    /// even on panic) stops the watchdog. A fire-and-forget watchdog that
    /// outlived its test could `process::exit` during a *later* test if the
    /// whole suite ran slower than the timeout (busy CI/swarm host), spuriously
    /// killing the run. The watchdog only aborts if the guard is still alive at
    /// the deadline (the test genuinely hung).
    struct WatchdogGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for WatchdogGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[must_use = "hold the guard for the duration of the test"]
    fn hang_watchdog(secs: u64, label: &'static str, exit_code: i32) -> WatchdogGuard {
        use std::sync::atomic::Ordering;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            for _ in 0..secs.saturating_mul(20) {
                std::thread::sleep(Duration::from_millis(50));
                if flag.load(Ordering::SeqCst) {
                    return;
                }
            }
            if !flag.swap(true, Ordering::SeqCst) {
                eprintln!("WATCHDOG: `{label}` HUNG after {secs}s");
                std::process::exit(exit_code);
            }
        });
        WatchdogGuard(done)
    }

    #[cfg(unix)]
    fn unique_handshake_socket_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let sequence = TEST_SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        PathBuf::from(format!(
            "/tmp/ftk4-{}-{nanos}-{sequence}.sock",
            std::process::id(),
        ))
    }

    #[derive(Debug)]
    struct RpcMetricProbe {
        pending: Arc<MetricAtomicU64>,
        admitted: Arc<MetricAtomicU64>,
        preclosed: Arc<MetricAtomicU64>,
        delivered: Arc<MetricAtomicU64>,
        abandoned: Arc<MetricAtomicU64>,
        transport_failed_live: Arc<MetricAtomicU64>,
        transport_cleared_abandoned: Arc<MetricAtomicU64>,
        retirement_reply_channel_full: Arc<MetricAtomicU64>,
        future_serial: Arc<MetricAtomicU64>,
        unmatched_serial: Arc<MetricAtomicU64>,
        unexpected_response_ident: Arc<MetricAtomicU64>,
        serial_exhausted: Arc<MetricAtomicU64>,
        reserve_failed: Arc<MetricAtomicU64>,
        serial_collision: Arc<MetricAtomicU64>,
        protocol_reply_channel_full: Arc<MetricAtomicU64>,
    }

    impl RpcMetricProbe {
        fn new() -> (RpcMetrics, Self) {
            let probe = Self {
                pending: Arc::new(MetricAtomicU64::new(0.0_f64.to_bits())),
                admitted: Arc::new(MetricAtomicU64::new(0)),
                preclosed: Arc::new(MetricAtomicU64::new(0)),
                delivered: Arc::new(MetricAtomicU64::new(0)),
                abandoned: Arc::new(MetricAtomicU64::new(0)),
                transport_failed_live: Arc::new(MetricAtomicU64::new(0)),
                transport_cleared_abandoned: Arc::new(MetricAtomicU64::new(0)),
                retirement_reply_channel_full: Arc::new(MetricAtomicU64::new(0)),
                future_serial: Arc::new(MetricAtomicU64::new(0)),
                unmatched_serial: Arc::new(MetricAtomicU64::new(0)),
                unexpected_response_ident: Arc::new(MetricAtomicU64::new(0)),
                serial_exhausted: Arc::new(MetricAtomicU64::new(0)),
                reserve_failed: Arc::new(MetricAtomicU64::new(0)),
                serial_collision: Arc::new(MetricAtomicU64::new(0)),
                protocol_reply_channel_full: Arc::new(MetricAtomicU64::new(0)),
            };
            let metrics = RpcMetrics {
                pending: Gauge::from_arc(Arc::clone(&probe.pending)),
                admitted: Counter::from_arc(Arc::clone(&probe.admitted)),
                preclosed: Counter::from_arc(Arc::clone(&probe.preclosed)),
                delivered: Counter::from_arc(Arc::clone(&probe.delivered)),
                abandoned: Counter::from_arc(Arc::clone(&probe.abandoned)),
                transport_failed_live: Counter::from_arc(Arc::clone(&probe.transport_failed_live)),
                transport_cleared_abandoned: Counter::from_arc(Arc::clone(
                    &probe.transport_cleared_abandoned,
                )),
                retirement_reply_channel_full: Counter::from_arc(Arc::clone(
                    &probe.retirement_reply_channel_full,
                )),
                future_serial: Counter::from_arc(Arc::clone(&probe.future_serial)),
                unmatched_serial: Counter::from_arc(Arc::clone(&probe.unmatched_serial)),
                unexpected_response_ident: Counter::from_arc(Arc::clone(
                    &probe.unexpected_response_ident,
                )),
                serial_exhausted: Counter::from_arc(Arc::clone(&probe.serial_exhausted)),
                reserve_failed: Counter::from_arc(Arc::clone(&probe.reserve_failed)),
                serial_collision: Counter::from_arc(Arc::clone(&probe.serial_collision)),
                protocol_reply_channel_full: Counter::from_arc(Arc::clone(
                    &probe.protocol_reply_channel_full,
                )),
            };
            (metrics, probe)
        }

        fn counter(counter: &MetricAtomicU64) -> u64 {
            counter.load(Ordering::Acquire)
        }

        fn pending(&self) -> f64 {
            let value = f64::from_bits(self.pending.load(Ordering::Acquire));
            assert!(
                value.is_finite() && value >= 0.0 && value.fract() == 0.0,
                "pending gauge must be a finite non-negative integer, got {}",
                value
            );
            value
        }

        fn assert_balanced(&self) {
            assert_eq!(
                self.pending(),
                0.0,
                "balance is asserted only at a quiescent boundary"
            );
            let retired = Self::counter(&self.delivered)
                + Self::counter(&self.abandoned)
                + Self::counter(&self.transport_failed_live)
                + Self::counter(&self.transport_cleared_abandoned)
                + Self::counter(&self.retirement_reply_channel_full);
            assert_eq!(
                Self::counter(&self.admitted),
                retired,
                "at quiescence every admitted request must have exactly one retirement outcome"
            );
        }
    }

    fn pending_replies_for_test() -> (PendingReplies, RpcMetricProbe) {
        let (metrics, probe) = RpcMetricProbe::new();
        (pending_replies_with_metrics(metrics), probe)
    }

    fn test_wire_ident<T: PduWireIdent>() -> NonZeroU64 {
        NonZeroU64::new(T::IDENT).expect("test PDU wire identifier must be nonzero")
    }

    fn pending_replies_with_metrics(metrics: RpcMetrics) -> PendingReplies {
        let rpc_transport = Arc::new(RpcTransportState::new());
        PendingReplies::new(
            metrics,
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
                .expect("initial test generation is nonzero"),
            rpc_transport,
        )
    }

    fn push_test_unsigned_leb128(mut value: u64, output: &mut Vec<u8>) {
        loop {
            let mut byte =
                u8::try_from(value & 0x7f).expect("masked test LEB128 byte must fit in u8");
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                return;
            }
        }
    }

    /// Build an intentionally schema-opaque frame. Ordered-window rejection
    /// must happen from this header alone, so none of these payload bytes may
    /// be interpreted, decompressed, or allocated by the PDU body decoder.
    fn test_opaque_frame(
        ident: u64,
        serial: u64,
        compressed: bool,
        payload: &[u8],
    ) -> (Vec<u8>, usize) {
        let mut encoded_serial = Vec::new();
        push_test_unsigned_leb128(serial, &mut encoded_serial);
        let mut encoded_ident = Vec::new();
        push_test_unsigned_leb128(ident, &mut encoded_ident);
        let body_len = encoded_serial
            .len()
            .checked_add(encoded_ident.len())
            .and_then(|len| len.checked_add(payload.len()))
            .expect("test frame length must not overflow");
        let mut tagged_len = u64::try_from(body_len).expect("test frame length must fit u64");
        if compressed {
            tagged_len |= 1_u64 << 63;
        }

        let mut frame = Vec::new();
        push_test_unsigned_leb128(tagged_len, &mut frame);
        frame.extend_from_slice(&encoded_serial);
        frame.extend_from_slice(&encoded_ident);
        let header_len = frame.len();
        frame.extend_from_slice(payload);
        (frame, header_len)
    }

    #[test]
    fn remote_rpc_error_is_finite_and_request_correlated() {
        let response = ErrorResponse::backend_failure(Ping::IDENT);
        let error = remote_rejection_error("ping", Ping::IDENT, &response).to_string();
        assert!(error.contains("code=backend_failure"));
        assert!(error.starts_with("FRANKENTERM_MUX_ERROR_V1 "));
        assert!(error.contains(&format!("request_ident={}", Ping::IDENT)));
        assert!(error.contains(&format!("response_request_ident={}", Ping::IDENT)));
        assert!(error.contains("operation=ping"));
        assert!(error.contains("object=none"));
        assert!(error.contains("effect=not_applied"));
        assert!(error.contains("retry=safe_after_backoff"));
        assert!(!error.contains("SECRET_REMOTE_STDERR_CANARY"));

        let mismatch = remote_rejection_error("ping", ListPanes::IDENT, &response).to_string();
        assert!(mismatch.contains("code=unknown_future"));
        assert!(mismatch.contains(&format!("request_ident={}", ListPanes::IDENT)));
        assert!(mismatch.contains(&format!("response_request_ident={}", Ping::IDENT)));
        assert!(mismatch.contains("effect=unknown_future"));
        assert!(mismatch.contains("retry=unknown_future"));
    }

    fn client_with_idle_rpc_queue() -> (Client, Receiver<ReaderMessage>) {
        client_with_idle_rpc_queue_and_limits(ClientOutboundBudgetLimits::default())
    }

    fn client_with_idle_rpc_queue_and_limits(
        limits: ClientOutboundBudgetLimits,
    ) -> (Client, Receiver<ReaderMessage>) {
        let (sender, receiver) = unbounded();
        client_with_rpc_queue_and_limits(sender, receiver, limits)
    }

    fn client_with_bounded_idle_rpc_queue(capacity: usize) -> (Client, Receiver<ReaderMessage>) {
        let (sender, receiver) = bounded(capacity);
        client_with_rpc_queue_and_limits(sender, receiver, ClientOutboundBudgetLimits::default())
    }

    fn client_with_rpc_queue_and_limits(
        sender: Sender<ReaderMessage>,
        receiver: Receiver<ReaderMessage>,
        limits: ClientOutboundBudgetLimits,
    ) -> (Client, Receiver<ReaderMessage>) {
        let rpc_transport = Arc::new(RpcTransportState::new_with_outbound_budget_limits(limits));
        rpc_transport.mark_current_generation_ready_for_test();
        (
            Client {
                sender,
                local_domain_id: None,
                incarnation: Arc::new(ClientIncarnation),
                connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
                rpc_transport,
                domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
                client_id: ClientId::new(),
                client_domain_config: ClientDomainConfig::Unix(UnixDomain::default()),
                is_reconnectable: false,
                is_local: true,
            },
            receiver,
        )
    }

    fn client_with_bootstrap_rpc_queue() -> (Client, Receiver<ReaderMessage>) {
        let (sender, receiver) = unbounded();
        (
            Client {
                sender,
                local_domain_id: None,
                incarnation: Arc::new(ClientIncarnation),
                connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
                rpc_transport: Arc::new(RpcTransportState::new()),
                domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
                client_id: ClientId::new(),
                client_domain_config: ClientDomainConfig::Unix(UnixDomain::default()),
                is_reconnectable: false,
                is_local: true,
            },
            receiver,
        )
    }

    #[cfg(unix)]
    struct RealHandshakeProbe {
        result: anyhow::Result<GetCodecVersionResponse>,
        transcript: Vec<&'static str>,
        codec: Option<RpcCodecAuthority>,
        phase: Option<RpcProtocolPhase>,
        ready_generation: u64,
        connection_generation: u64,
    }

    /// Drive the ambient `Client::verify_version_compat` path against the real
    /// reader and a kernel socket pair. This is intentionally stronger than a
    /// direct `RpcCodecAuthority::negotiate` unit test: both bootstrap requests
    /// must pass through the queue, serial allocator, frame codec, socket, and
    /// inbound correlation path before readiness can publish.
    #[cfg(unix)]
    fn run_real_socket_pair_handshake(
        remote_max: usize,
        remote_min: usize,
        expect_registration: bool,
    ) -> RealHandshakeProbe {
        let _watchdog = hang_watchdog(15, "real socket-pair codec handshake", 93);
        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create codec-handshake socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound codec-handshake server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("bound codec-handshake server writes");
        let (server_release_tx, server_release_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-codec-window-server".to_string())
            .spawn(move || -> anyhow::Result<Vec<&'static str>> {
                let mut transcript = Vec::new();
                let request = Pdu::decode(&mut server_stream)
                    .context("decode socket-pair GetCodecVersion")?;
                transcript.push(request.pdu.pdu_name());
                anyhow::ensure!(
                    matches!(request.pdu, Pdu::GetCodecVersion(_)),
                    "bootstrap did not begin with GetCodecVersion"
                );
                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: remote_max,
                    version_string: format!("socket-pair-codec-{remote_min}-{remote_max}"),
                    executable_path: PathBuf::from("/test/frankenterm-mux-server"),
                    config_file_path: None,
                    min_supported: remote_min,
                })
                .encode(&mut server_stream, request.serial)
                .context("encode socket-pair GetCodecVersionResponse")?;
                Write::flush(&mut server_stream)
                    .context("flush socket-pair GetCodecVersionResponse")?;

                if expect_registration {
                    let request = Pdu::decode(&mut server_stream)
                        .context("decode socket-pair SetClientId")?;
                    transcript.push(request.pdu.pdu_name());
                    anyhow::ensure!(
                        matches!(request.pdu, Pdu::SetClientId(_)),
                        "compatible bootstrap did not continue with SetClientId"
                    );
                    Pdu::UnitResponse(UnitResponse {})
                        .encode(&mut server_stream, request.serial)
                        .context("encode socket-pair registration response")?;
                    Write::flush(&mut server_stream)
                        .context("flush socket-pair registration response")?;
                }

                server_release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .context("hold socket-pair peer through client assertions")?;

                if !expect_registration && server_stream.try_readable_without_consuming()? {
                    match Pdu::decode(&mut server_stream) {
                        Ok(unexpected) => transcript.push(unexpected.pdu.pdu_name()),
                        Err(error)
                            if error
                                .root_cause()
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io| io.kind() == ErrorKind::UnexpectedEof) => {}
                        Err(error) => {
                            return Err(error)
                                .context("probe rejected-handshake socket for a request prefix");
                        }
                    }
                }
                Ok(transcript)
            })
            .expect("spawn socket-pair codec server");

        let client_domain_config = ClientDomainConfig::Unix(UnixDomain {
            name: "ft-codec-window-test".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        let reconnectable =
            Reconnectable::new(client_domain_config.clone(), Some(Box::new(client_stream)));
        let (sender, receiver) = unbounded();
        let client = Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport: Arc::new(RpcTransportState::new()),
            domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable: false,
            is_local: true,
        };
        let dispatch_authority = client.test_dispatch_authority(Weak::new());
        let reader = std::thread::Builder::new()
            .name("ft-codec-window-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn socket-pair codec reader");

        let ui = ConnectionUI::new_headless();
        let result = asupersync_block_on(client.verify_version_compat(&ui));
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("initial generation is nonzero");
        let (codec, phase) = {
            let lifecycle = client.rpc_transport.lifecycle.lock();
            (
                lifecycle.protocol.and_then(|protocol| protocol.codec),
                lifecycle.protocol.map(|protocol| protocol.phase),
            )
        };
        let ready_generation = client
            .rpc_transport
            .ready_generation
            .load(AtomicOrdering::Acquire);
        assert_eq!(codec, client.rpc_transport.codec_authority(generation));
        let connection_generation = Arc::clone(&client.connection_generation);

        server_release_tx
            .send(())
            .expect("release socket-pair codec server");
        drop(client);
        assert!(
            reader
                .join()
                .expect("socket-pair codec reader thread panicked")
                .is_err(),
            "closing the test socket must terminate its reader"
        );
        let transcript = server
            .join()
            .expect("socket-pair codec server thread panicked")
            .expect("socket-pair codec server failed");

        RealHandshakeProbe {
            result,
            transcript,
            codec,
            phase,
            ready_generation,
            connection_generation: connection_generation.load(AtomicOrdering::Acquire),
        }
    }

    fn advance_bootstrap_to_registration_request(client: &Client) -> NonZeroU64 {
        let generation = client
            .rpc_transport
            .active_generation()
            .expect("bootstrap queue transport starts live");
        let response = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "registration-rollback-test".to_string(),
            executable_path: PathBuf::from("/test/frankenterm-mux-server"),
            config_file_path: None,
            min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
        });
        let mut lifecycle = client.rpc_transport.lifecycle.lock();
        let protocol = lifecycle
            .protocol_for_mut(generation)
            .expect("bootstrap protocol authority is live");
        protocol
            .admit_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
            .expect("admit prerequisite codec request");
        protocol
            .complete_correlated_response("GetCodecVersion", &response)
            .expect("complete prerequisite codec negotiation");
        assert_eq!(
            protocol.phase,
            RpcProtocolPhase::AwaitingRegistrationRequest
        );
        generation
    }

    fn pending_readiness_scope_for_test() -> (
        RpcGenerationScope,
        Receiver<ReaderMessage>,
        Arc<RpcTransportState>,
    ) {
        let (sender, receiver) = unbounded();
        let rpc_transport = Arc::new(RpcTransportState::new());
        let scope = RpcGenerationScope::bootstrap(sender, Arc::clone(&rpc_transport));
        assert!(scope.is_available());
        (scope, receiver, rpc_transport)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ScriptedFailureBoundary {
        PartialWrite { accepted_prefix: usize },
        Flush,
        AwaitingResponseEof,
    }

    struct ScriptedTransportState {
        transcript: StdMutex<Vec<u8>>,
        write_calls: AtomicU64,
        read_ready: AtomicBool,
        read_waker: futures::task::AtomicWaker,
    }

    impl ScriptedTransportState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                transcript: StdMutex::new(Vec::new()),
                write_calls: AtomicU64::new(0),
                read_ready: AtomicBool::new(false),
                read_waker: futures::task::AtomicWaker::new(),
            })
        }

        fn transcript(&self) -> Vec<u8> {
            self.transcript
                .lock()
                .expect("scripted transport transcript lock")
                .clone()
        }
    }

    struct ScriptedTransport {
        boundary: ScriptedFailureBoundary,
        state: Arc<ScriptedTransportState>,
    }

    impl fmt::Debug for ScriptedTransport {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt.debug_struct("ScriptedTransport")
                .field("boundary", &self.boundary)
                .finish_non_exhaustive()
        }
    }

    impl AsyncRead for ScriptedTransport {
        fn poll_read(
            self: Pin<&mut Self>,
            _task_cx: &mut TaskContext<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.boundary == ScriptedFailureBoundary::AwaitingResponseEof
                && self.state.read_ready.load(Ordering::Acquire)
            {
                // A successful zero-byte read is EOF. The peer observed the
                // complete request frame but deliberately withheld its reply.
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

    impl AsyncWrite for ScriptedTransport {
        fn poll_write(
            self: Pin<&mut Self>,
            _task_cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            match self.boundary {
                ScriptedFailureBoundary::PartialWrite { accepted_prefix } => {
                    let write_call = self.state.write_calls.fetch_add(1, Ordering::AcqRel);
                    if write_call == 0 {
                        let accepted = accepted_prefix.min(buf.len());
                        assert!(
                            accepted > 0 && accepted < buf.len(),
                            "partial-write script needs a strict non-empty frame prefix"
                        );
                        self.state
                            .transcript
                            .lock()
                            .expect("scripted transport transcript lock")
                            .extend_from_slice(&buf[..accepted]);
                        Poll::Ready(Ok(accepted))
                    } else {
                        Poll::Ready(Err(std::io::Error::new(
                            ErrorKind::BrokenPipe,
                            "scripted failure after a partial frame write",
                        )))
                    }
                }
                ScriptedFailureBoundary::Flush | ScriptedFailureBoundary::AwaitingResponseEof => {
                    self.state.write_calls.fetch_add(1, Ordering::AcqRel);
                    self.state
                        .transcript
                        .lock()
                        .expect("scripted transport transcript lock")
                        .extend_from_slice(buf);
                    Poll::Ready(Ok(buf.len()))
                }
            }
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _task_cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            match self.boundary {
                ScriptedFailureBoundary::Flush => Poll::Ready(Err(std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "scripted flush failure after a complete frame write",
                ))),
                ScriptedFailureBoundary::AwaitingResponseEof => {
                    self.state.read_ready.store(true, Ordering::Release);
                    self.state.read_waker.wake();
                    Poll::Ready(Ok(()))
                }
                ScriptedFailureBoundary::PartialWrite { .. } => Poll::Ready(Ok(())),
            }
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _task_cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[async_trait]
    impl AsyncReadAndWrite for ScriptedTransport {
        async fn wait_for_readable(&self) -> anyhow::Result<()> {
            poll_fn(|task_cx| {
                self.state.read_waker.register(task_cx.waker());
                if self.state.read_ready.load(Ordering::Acquire) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
        }
    }

    fn run_scripted_rpc_failure(
        pdu: Pdu,
        boundary: ScriptedFailureBoundary,
    ) -> (anyhow::Error, anyhow::Error, Vec<u8>) {
        let _watchdog = hang_watchdog(15, "scripted RPC failure boundary", 94);
        let (client, mut receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let state = ScriptedTransportState::new();
        let stream = ScriptedTransport {
            boundary,
            state: Arc::clone(&state),
        };
        let mut reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(UnixDomain::default()),
            Some(Box::new(stream)),
        );
        let rpc = client.send_pdu(pdu);

        let (reader_result, rpc_result) = asupersync_block_on(futures::future::join(
            client_thread_async(&mut reconnectable, &mut receiver, &authority),
            rpc,
        ));
        (
            reader_result.expect_err("the scripted reader must terminate at its boundary"),
            rpc_result.expect_err("the RPC must receive one typed transport failure"),
            state.transcript(),
        )
    }

    fn assert_rpc_retirement(
        error: &anyhow::Error,
        expected_stage: RpcRetirementStage,
        expected_certainty: RpcDeliveryCertainty,
    ) {
        assert!(
            matches!(
                error.downcast_ref::<RpcTransportError>(),
                Some(RpcTransportError::Retired {
                    stage,
                    certainty,
                    ..
                }) if *stage == expected_stage && *certainty == expected_certainty
            ),
            "unexpected RPC retirement: {:#}",
            error
        );
    }

    #[test]
    fn scripted_transport_boundaries_preserve_delivery_certainty_and_wire_evidence() {
        let (_reader_error, rpc_error, transcript) = run_scripted_rpc_failure(
            Pdu::Ping(Ping {}),
            ScriptedFailureBoundary::PartialWrite { accepted_prefix: 1 },
        );
        assert_rpc_retirement(
            &rpc_error,
            RpcRetirementStage::WriteStarted,
            RpcDeliveryCertainty::OutcomeUnknown,
        );
        assert_eq!(
            transcript.len(),
            1,
            "partial-write witness must retain the exact accepted prefix"
        );
        assert!(
            Pdu::decode(std::io::Cursor::new(&transcript)).is_err(),
            "a strict frame prefix must not decode as a complete request"
        );

        for (boundary, expected_stage) in [
            (
                ScriptedFailureBoundary::Flush,
                RpcRetirementStage::BeforeFlush,
            ),
            (
                ScriptedFailureBoundary::AwaitingResponseEof,
                RpcRetirementStage::AwaitingResponse,
            ),
        ] {
            let (_reader_error, rpc_error, transcript) =
                run_scripted_rpc_failure(Pdu::Ping(Ping {}), boundary);
            assert_rpc_retirement(
                &rpc_error,
                expected_stage,
                RpcDeliveryCertainty::OutcomeUnknown,
            );
            let decoded = Pdu::decode(std::io::Cursor::new(&transcript))
                .expect("post-write failure transcript must contain one complete request");
            assert_eq!(decoded.serial, 1);
            assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
        }
    }

    #[test]
    fn frame_encoding_failure_retires_one_pending_caller_before_a_frame_exists() {
        // Every currently activated ordinary request has an infallible schema,
        // while `Invalid` is correctly stopped by the earlier protocol gate.
        // Start at the reader's already-admitted FrameEncoding boundary so the
        // test can exercise the real codec failure plus the exact production
        // pending-retirement path without weakening admission or adding a
        // failpoint. `encode_frame` returning `Err` means there is no frame that
        // could be handed to `write_all`, which is the zero-wire guarantee.
        let (client, receiver) = client_with_idle_rpc_queue();
        let generation = client
            .rpc_transport
            .active_generation()
            .expect("frame-encoding test transport starts live");
        let authority = client.test_dispatch_authority(Weak::new());
        let (metrics, probe) = RpcMetricProbe::new();
        let mut pending =
            PendingReplies::new(metrics, generation, Arc::clone(&client.rpc_transport));
        let (completion_tx, completion_rx) = bounded(1);
        let serial = pending
            .admit_named(completion_tx, "Invalid")
            .expect("admit frame-encoding boundary request")
            .expect("live caller receives one wire serial");
        pending
            .set_stage(serial, RpcRetirementStage::FrameEncoding)
            .expect("record the real pre-write encoding boundary");

        let encode_error = Pdu::Invalid { ident: 0xdead_beef }
            .encode_frame(serial.get())
            .expect_err("Invalid must fail before producing any frame")
            .context("encoding a PDU frame to send to the server");
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION
        );
        authority
            .begin_rpc_transport_retirement()
            .expect("encoding failure retires the exact generation");
        pending.fail_after_transport_error(&encode_error);

        let caller_error = completion_rx
            .try_recv()
            .expect("encoding failure must settle the admitted caller")
            .expect_err("encoding failure must not synthesize a response");
        assert_rpc_retirement(
            &caller_error,
            RpcRetirementStage::FrameEncoding,
            RpcDeliveryCertainty::DefinitelyNotSent,
        );
        assert!(matches!(
            completion_rx.try_recv(),
            Err(async_channel::TryRecvError::Closed)
        ));
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 1);
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
        assert_eq!(client.rpc_transport.active_generation(), None);
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "retirement must not itself publish the planned successor"
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn unknown_and_above_dialect_outbound_pdus_stop_before_queue_serial_and_wire() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let generation = client
            .rpc_transport
            .active_generation()
            .expect("local-gate test transport starts live");
        let synthetic_agreed = LEGACY46_CODEC_VERSION;
        client.rpc_transport.lifecycle.lock().protocol = Some(
            RpcProtocolAuthority::established_for_test(generation, synthetic_agreed),
        );
        let serial_before = client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);
        let error = asupersync_block_on(client.send_pdu(Pdu::Invalid { ident: 0xdead_beef }))
            .expect_err("an unassigned PDU must fail at local preflight");

        assert_eq!(
            error.downcast_ref::<OrdinaryMuxProtocolError>(),
            Some(&OrdinaryMuxProtocolError::UnknownPdu {
                direction: RpcProtocolDirection::Outbound,
                ident: 0xdead_beef,
            })
        );
        let above_dialect =
            asupersync_block_on(client.send_pdu(Pdu::ListPanesCoherent(ListPanesCoherent {
                supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            })))
            .expect_err(
                "a request absent from the exact legacy dialect must fail at local preflight",
            );
        assert!(matches!(
            above_dialect.downcast_ref::<OrdinaryMuxProtocolError>(),
            Some(OrdinaryMuxProtocolError::DialectViolation {
                ident,
                required,
                agreed,
                ..
            }) if *ident == <ListPanesCoherent as PduWireIdent>::IDENT
                && *required
                    == <ListPanesCoherent as PduWireIdent>::WIRE_SPEC.min_codec_version
                && *agreed == synthetic_agreed
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert_eq!(
            client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before,
            "local rejection must not allocate a wire serial"
        );

        let (response, ()) = asupersync_block_on(futures::future::join(
            client.send_pdu(Pdu::Ping(Ping {})),
            async {
                let message = receiver
                    .recv()
                    .await
                    .expect("a valid successor request must still enqueue");
                let ReaderMessage::SendPdu { lease, promise, .. } = message else {
                    panic!("valid successor must enqueue as a reader PDU");
                };
                let prepared = lease
                    .claim_for_reader()
                    .expect("valid successor reader claim should remain coherent")
                    .expect("valid successor request must retain its exact PDU");
                assert!(matches!(prepared.pdu(), Pdu::Ping(_)));
                promise
                    .send(Ok(PendingRpcReply::pdu(Pdu::Pong(Pong {}))))
                    .await
                    .expect("valid successor caller must remain live");
            },
        ));
        assert_eq!(
            response.expect("connection must remain reusable"),
            Pdu::Pong(Pong {})
        );
    }

    #[test]
    fn codec_authority_negotiation_covers_current_legacy_and_invalid_windows() {
        let generation = NonZeroU64::new(7).expect("test generation is nonzero");
        let disjoint_max = codec::CODEC_VERSION_MIN_SUPPORTED
            .checked_sub(1)
            .expect("the supported codec window has a lower disjoint version");
        let current_peer = RpcCodecAuthority::negotiate(
            generation,
            CODEC_VERSION,
            codec::CODEC_VERSION_MIN_SUPPORTED,
        )
        .expect("current client must overlap the current peer");
        assert_eq!(current_peer.agreed, CODEC_VERSION);

        let next_peer = RpcCodecAuthority::negotiate(generation, CODEC_VERSION + 1, CODEC_VERSION)
            .expect("current client must overlap a current-plus-one peer");
        assert_eq!(next_peer.agreed, CODEC_VERSION);

        let legacy = RpcCodecAuthority::negotiate(generation, CODEC_VERSION, 0)
            .expect("legacy min zero must conservatively mean remote max only");
        assert_eq!(legacy.remote_min, CODEC_VERSION);
        assert_eq!(legacy.agreed, CODEC_VERSION);

        let legacy46 = RpcCodecAuthority::negotiate(generation, LEGACY46_CODEC_VERSION, 0)
            .expect("the exact frozen codec-46 dialect must remain reconnectable");
        assert_eq!(legacy46.remote_min, LEGACY46_CODEC_VERSION);
        assert_eq!(legacy46.agreed, LEGACY46_CODEC_VERSION);
        assert_eq!(legacy46.dialect, MuxWireDialect::LEGACY46);

        assert!(RpcCodecAuthority::negotiate(generation, disjoint_max, disjoint_max).is_err());
        assert!(RpcCodecAuthority::negotiate(generation, disjoint_max, CODEC_VERSION).is_err());
        assert!(
            RpcCodecAuthority::negotiate(generation, CODEC_VERSION + 1, CODEC_VERSION + 1,)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn ambient_real_handshake_covers_current_legacy_and_disjoint_codec_windows() {
        let disjoint_max = codec::CODEC_VERSION_MIN_SUPPORTED
            .checked_sub(1)
            .expect("the supported codec window has a lower disjoint version");
        for (advertised_min, retained_min, label) in [
            (
                codec::CODEC_VERSION_MIN_SUPPORTED,
                codec::CODEC_VERSION_MIN_SUPPORTED,
                "explicit-overlap",
            ),
            (0, CODEC_VERSION, "legacy-min-zero"),
        ] {
            let RealHandshakeProbe {
                result,
                transcript,
                codec,
                phase,
                ready_generation,
                connection_generation,
            } = run_real_socket_pair_handshake(CODEC_VERSION, advertised_min, true);
            let info = result.unwrap_or_else(|error| {
                panic!("{} real handshake failed unexpectedly: {:#}", label, error)
            });
            assert_eq!(info.codec_vers, CODEC_VERSION, "{label}");
            assert_eq!(info.min_supported, advertised_min, "{label}");
            assert_eq!(transcript, ["GetCodecVersion", "SetClientId"], "{label}");
            let codec = codec.unwrap_or_else(|| panic!("{} lost codec authority", label));
            assert_eq!(codec.remote_max, CODEC_VERSION, "{label}");
            assert_eq!(codec.remote_min, retained_min, "{label}");
            assert_eq!(codec.agreed, CODEC_VERSION, "{label}");
            assert_eq!(phase, Some(RpcProtocolPhase::Established), "{label}");
            assert_eq!(ready_generation, INITIAL_CONNECTION_GENERATION, "{label}");
            assert_eq!(
                connection_generation, INITIAL_CONNECTION_GENERATION,
                "{label} must not mint a successor generation"
            );
        }

        let RealHandshakeProbe {
            result,
            transcript,
            codec,
            phase,
            ready_generation,
            connection_generation,
        } = run_real_socket_pair_handshake(LEGACY46_CODEC_VERSION, LEGACY46_CODEC_VERSION, true);
        let info = result.expect("exact codec-46 socket handshake must reconnect");
        assert_eq!(info.codec_vers, LEGACY46_CODEC_VERSION);
        assert_eq!(info.min_supported, LEGACY46_CODEC_VERSION);
        assert_eq!(transcript, ["GetCodecVersion", "SetClientId"]);
        let codec = codec.expect("exact codec-46 handshake must retain codec authority");
        assert_eq!(codec.remote_max, LEGACY46_CODEC_VERSION);
        assert_eq!(codec.remote_min, LEGACY46_CODEC_VERSION);
        assert_eq!(codec.agreed, LEGACY46_CODEC_VERSION);
        assert_eq!(codec.dialect, MuxWireDialect::LEGACY46);
        assert_eq!(phase, Some(RpcProtocolPhase::Established));
        assert_eq!(ready_generation, INITIAL_CONNECTION_GENERATION);
        assert_eq!(connection_generation, INITIAL_CONNECTION_GENERATION);

        for (remote_max, remote_min, label) in [
            (disjoint_max, disjoint_max, "lower-disjoint-window"),
            (disjoint_max, CODEC_VERSION, "impossible-window"),
            (CODEC_VERSION + 1, CODEC_VERSION + 1, "disjoint-window"),
        ] {
            let RealHandshakeProbe {
                result,
                transcript,
                codec,
                phase,
                ready_generation,
                connection_generation,
            } = run_real_socket_pair_handshake(remote_max, remote_min, false);
            let error = result.expect_err("an incompatible real handshake must fail");
            assert!(
                error.downcast_ref::<IncompatibleVersionError>().is_some(),
                "{} lost its typed incompatibility: {:#}",
                label,
                error
            );
            assert_eq!(transcript, ["GetCodecVersion"], "{label}");
            assert_eq!(codec, None, "{label} retained invalid codec authority");
            assert_ne!(
                phase,
                Some(RpcProtocolPhase::Established),
                "{label} established ordinary traffic"
            );
            assert_eq!(ready_generation, 0, "{label} published readiness");
            assert_eq!(
                connection_generation, INITIAL_CONNECTION_GENERATION,
                "{label} must not publish a successor generation"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_legacy46_socket_handshake_and_topology_commit_use_one_live_generation() {
        fn append_leb128(mut value: u64, target: &mut Vec<u8>) {
            loop {
                let mut byte = (value & 0x7f) as u8;
                value >>= 7;
                if value != 0 {
                    byte |= 0x80;
                }
                target.push(byte);
                if value == 0 {
                    return;
                }
            }
        }

        fn empty_legacy46_topology_frame(serial: u64) -> Vec<u8> {
            // Exact v46 PDU4 body: three empty positional fields
            // (tabs, tab_titles, window_titles), with no fourth floating-pane
            // vector. Build the frame independently from the current PDU4
            // encoder so this test cannot accidentally serialize that field.
            let body = [0_u8, 0_u8, 0_u8];
            let mut serial_field = Vec::new();
            append_leb128(serial, &mut serial_field);
            let ident = <ListPanesResponse as PduWireIdent>::IDENT;
            let mut ident_field = Vec::new();
            append_leb128(ident, &mut ident_field);
            let tagged_len = serial_field
                .len()
                .checked_add(ident_field.len())
                .and_then(|bytes| bytes.checked_add(body.len()))
                .and_then(|bytes| u64::try_from(bytes).ok())
                .expect("small legacy topology frame length must fit");
            let mut frame = Vec::new();
            append_leb128(tagged_len, &mut frame);
            frame.extend(serial_field);
            frame.extend(ident_field);
            frame.extend(body);
            frame
        }

        let _watchdog = hang_watchdog(20, "exact codec-46 topology socket", 99);
        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create exact codec-46 socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound codec-46 server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("bound codec-46 server writes");
        let (release_server_tx, release_server_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-exact-codec46-topology-server".to_string())
            .spawn(move || -> anyhow::Result<Vec<&'static str>> {
                let mut transcript = Vec::new();
                let bootstrap =
                    Pdu::decode_for_dialect(&mut server_stream, MuxWireDialect::LEGACY46)
                        .context("decode exact codec-46 GetCodecVersion")?;
                let (bootstrap_serial, MuxWireDecodedPayload::Pdu(bootstrap)) =
                    bootstrap.into_parts()
                else {
                    bail!("codec-46 bootstrap decoded to a changed legacy schema");
                };
                anyhow::ensure!(
                    matches!(bootstrap, Pdu::GetCodecVersion(_)),
                    "codec-46 bootstrap did not begin with GetCodecVersion"
                );
                transcript.push("GetCodecVersion");
                let version_frame = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: LEGACY46_CODEC_VERSION,
                    version_string: "captured-live-compatible-codec46".to_string(),
                    executable_path: PathBuf::from("/test/frankenterm-mux-server-v46"),
                    config_file_path: None,
                    min_supported: LEGACY46_CODEC_VERSION,
                })
                .prepare_outbound_for_dialect(
                    MuxWireDialect::LEGACY46,
                    PduProducer::Server,
                    PduWireRole::CorrelatedReply,
                    Some(&<GetCodecVersion as PduWireIdent>::WIRE_SPEC),
                    CompressionMode::Never,
                )
                .context("plan exact codec-46 version response")?
                .encode_frame(bootstrap_serial)
                .context("encode exact codec-46 version response")?;
                Write::write_all(&mut server_stream, &version_frame)
                    .context("write exact codec-46 version response")?;
                Write::flush(&mut server_stream)
                    .context("flush exact codec-46 version response")?;

                let registration =
                    Pdu::decode_for_dialect(&mut server_stream, MuxWireDialect::LEGACY46)
                        .context("decode exact codec-46 SetClientId")?;
                let (registration_serial, MuxWireDecodedPayload::Pdu(registration)) =
                    registration.into_parts()
                else {
                    bail!("codec-46 registration decoded to a changed legacy schema");
                };
                anyhow::ensure!(
                    matches!(registration, Pdu::SetClientId(_)),
                    "codec-46 bootstrap did not continue with SetClientId"
                );
                transcript.push("SetClientId");
                let registration_frame = Pdu::UnitResponse(UnitResponse {})
                    .prepare_outbound_for_dialect(
                        MuxWireDialect::LEGACY46,
                        PduProducer::Server,
                        PduWireRole::CorrelatedReply,
                        Some(&<SetClientId as PduWireIdent>::WIRE_SPEC),
                        CompressionMode::Never,
                    )
                    .context("plan exact codec-46 registration response")?
                    .encode_frame(registration_serial)
                    .context("encode exact codec-46 registration response")?;
                Write::write_all(&mut server_stream, &registration_frame)
                    .context("write exact codec-46 registration response")?;
                Write::flush(&mut server_stream)
                    .context("flush exact codec-46 registration response")?;

                let topology =
                    Pdu::decode_for_dialect(&mut server_stream, MuxWireDialect::LEGACY46)
                        .context("decode exact codec-46 ListPanes")?;
                let (topology_serial, MuxWireDecodedPayload::Pdu(topology)) = topology.into_parts()
                else {
                    bail!("codec-46 topology request decoded to a changed legacy schema");
                };
                anyhow::ensure!(
                    matches!(topology, Pdu::ListPanes(_)),
                    "codec-46 topology did not use ListPanes"
                );
                transcript.push("ListPanes");
                let topology_frame = empty_legacy46_topology_frame(topology_serial);
                Write::write_all(&mut server_stream, &topology_frame)
                    .context("write exact three-field codec-46 topology response")?;
                Write::flush(&mut server_stream)
                    .context("flush exact three-field codec-46 topology response")?;

                release_server_rx
                    .recv_timeout(Duration::from_secs(10))
                    .context("hold exact codec-46 socket through client assertions")?;
                Ok(transcript)
            })
            .expect("spawn exact codec-46 topology server");

        let client_domain_config = ClientDomainConfig::Unix(UnixDomain {
            name: "ft-exact-codec46-topology".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ..Default::default()
        });
        let reconnectable = Reconnectable::new(client_domain_config, Some(Box::new(client_stream)));
        let client = Client::new(None, reconnectable, Weak::new());
        let ui = ConnectionUI::new_headless();
        let applied = asupersync_block_on(async {
            let info = client.verify_version_compat(&ui).await?;
            anyhow::ensure!(info.codec_vers == LEGACY46_CODEC_VERSION);
            client
                .rpc_scope()
                .with_coherent_topology_snapshot(RpcConsumerKind::TopologySnapshot, |snapshot| {
                    match snapshot {
                        RpcTopologySnapshot::Legacy46(topology) => {
                            anyhow::ensure!(topology.tabs().is_empty());
                            anyhow::ensure!(topology.tab_titles().is_empty());
                            anyhow::ensure!(topology.window_titles().is_empty());
                            anyhow::ensure!(
                                topology.floating_pane_state()
                                    == Legacy46FloatingPaneState::Unavailable
                            );
                            Ok("legacy46-snapshot-committed")
                        }
                        RpcTopologySnapshot::Current(_) => {
                            bail!("exact codec-46 socket selected the current topology schema")
                        }
                    }
                })
                .await
        })
        .expect("exact codec-46 handshake and local topology commit must complete");
        assert_eq!(applied, "legacy46-snapshot-committed");
        let generation = NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
            .expect("initial connection generation is nonzero");
        let codec = client
            .rpc_transport
            .codec_authority(generation)
            .expect("exact codec-46 authority must remain live after topology commit");
        assert_eq!(codec.dialect, MuxWireDialect::LEGACY46);
        assert_eq!(
            client.connection_generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "successful codec-46 topology must not mint a reconnect generation"
        );

        release_server_tx
            .send(())
            .expect("release exact codec-46 test server");
        let transcript = server
            .join()
            .expect("exact codec-46 server thread panicked")
            .expect("exact codec-46 server failed");
        assert_eq!(transcript, ["GetCodecVersion", "SetClientId", "ListPanes"]);
        drop(client);
    }

    #[test]
    fn reconnect_replaces_codec_authority_instead_of_leaking_the_retired_window() {
        let (sender, receiver) = unbounded();
        let rpc_transport = Arc::new(RpcTransportState::new());
        let first_generation = rpc_transport
            .active_generation()
            .expect("first generation starts live");
        let first_scope = RpcGenerationScope::bootstrap(sender.clone(), Arc::clone(&rpc_transport));
        rpc_transport
            .lifecycle
            .lock()
            .protocol_for_mut(first_generation)
            .expect("first protocol authority is live")
            .admit_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
            .expect("first generation admits its codec request");
        let first_codec = RpcCodecAuthority::negotiate(
            first_generation,
            CODEC_VERSION,
            codec::CODEC_VERSION_MIN_SUPPORTED,
        )
        .expect("first peer overlaps locally");
        first_scope
            .retain_codec_authority(first_codec)
            .expect("first generation retains its codec window");
        assert_eq!(first_scope.codec_authority(), Some(first_codec));

        let dispatch = ClientDispatchAuthority::new(
            None,
            Weak::new(),
            Arc::new(ClientIncarnation),
            Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            Arc::clone(&rpc_transport),
        );
        let successor = dispatch
            .advance_generation(&receiver)
            .expect("retire first codec generation");
        successor
            .activate_rpc_transport()
            .expect("activate successor codec generation");
        let successor_generation =
            NonZeroU64::new(successor.generation).expect("successor generation is nonzero");
        let successor_scope = RpcGenerationScope::exact(
            sender,
            Arc::clone(&rpc_transport),
            successor_generation,
            true,
        );
        assert_eq!(first_scope.codec_authority(), None);
        assert_eq!(successor_scope.codec_authority(), None);

        rpc_transport
            .lifecycle
            .lock()
            .protocol_for_mut(successor_generation)
            .expect("successor protocol authority is live")
            .admit_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
            .expect("successor admits its own codec request");
        let successor_codec =
            RpcCodecAuthority::negotiate(successor_generation, CODEC_VERSION + 1, CODEC_VERSION)
                .expect("successor peer overlaps locally");
        successor_scope
            .retain_codec_authority(successor_codec)
            .expect("successor retains only its codec window");
        assert_eq!(successor_scope.codec_authority(), Some(successor_codec));
        assert_eq!(first_scope.codec_authority(), None);
    }

    #[test]
    fn protocol_bootstrap_is_exactly_codec_then_registration() {
        let generation = NonZeroU64::new(11).expect("test generation is nonzero");
        let mut protocol = RpcProtocolAuthority::new(generation);
        let ping = Pdu::Ping(Ping {});
        assert!(matches!(
            protocol.validate_outbound_pdu(&ping, RpcOutboundAdmissionPoint::Preflight),
            Err(OrdinaryMuxProtocolError::PhaseViolation { .. })
        ));

        protocol
            .admit_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
            .expect("codec request reaches enqueue authority");
        protocol
            .rollback_unadmitted_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
            .expect("preclosed codec request rolls its phase back");
        assert_eq!(protocol.phase, RpcProtocolPhase::AwaitingCodecRequest);
        assert_eq!(
            protocol
                .admit_outbound(&Pdu::GetCodecVersion(GetCodecVersion {}))
                .expect("codec request is the only first request"),
            RpcProtocolTransition::CodecRequest
        );
        assert!(matches!(
            protocol.validate_outbound_pdu(
                &Pdu::GetCodecVersion(GetCodecVersion {}),
                RpcOutboundAdmissionPoint::Preflight,
            ),
            Err(OrdinaryMuxProtocolError::PhaseViolation { .. })
        ));
        let codec = RpcCodecAuthority::negotiate(
            generation,
            CODEC_VERSION,
            codec::CODEC_VERSION_MIN_SUPPORTED,
        )
        .expect("current codec window overlaps itself");
        protocol
            .complete_correlated_response(
                "GetCodecVersion",
                &Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION,
                    version_string: "registry-bootstrap-test".to_string(),
                    executable_path: PathBuf::from("/test/ft"),
                    config_file_path: None,
                    min_supported: codec::CODEC_VERSION_MIN_SUPPORTED,
                }),
            )
            .expect("reader response advances bootstrap before another frame");
        protocol
            .install_codec(codec)
            .expect("caller-side retention of the same authority is idempotent");
        assert!(matches!(
            protocol.validate_inbound(
                &<NotifyAlert as PduWireIdent>::WIRE_SPEC,
                PduWireRole::Unilateral,
            ),
            Err(OrdinaryMuxProtocolError::PhaseViolation {
                phase: RpcProtocolPhase::AwaitingRegistrationRequest,
                ..
            })
        ));
        assert_eq!(
            protocol
                .admit_outbound(&Pdu::SetClientId(SetClientId {
                    client_id: ClientId::new(),
                    is_proxy: false,
                }))
                .expect("registration is the only second request"),
            RpcProtocolTransition::RegistrationRequest
        );
        assert!(matches!(
            protocol.validate_inbound(
                &<NotifyAlert as PduWireIdent>::WIRE_SPEC,
                PduWireRole::Unilateral,
            ),
            Err(OrdinaryMuxProtocolError::PhaseViolation {
                phase: RpcProtocolPhase::AwaitingRegistrationResponse,
                ..
            })
        ));
        protocol
            .validate_inbound(
                &<UnitResponse as PduWireIdent>::WIRE_SPEC,
                PduWireRole::CorrelatedReply,
            )
            .expect("registration unit response is authorized");
        protocol
            .complete_registration("SetClientId", &Pdu::UnitResponse(UnitResponse {}))
            .expect("unit response establishes ordinary traffic");
        protocol
            .validate_outbound_pdu(&ping, RpcOutboundAdmissionPoint::Preflight)
            .expect("ordinary request is legal after registration");
        assert!(matches!(
            protocol.validate_outbound_pdu(
                &Pdu::GetCodecVersion(GetCodecVersion {}),
                RpcOutboundAdmissionPoint::Preflight,
            ),
            Err(OrdinaryMuxProtocolError::PhaseViolation { .. })
        ));
    }

    #[test]
    fn registration_transition_rolls_back_for_queue_rejection_and_preclosed_dequeue() {
        let (rejected_client, rejected_receiver) = client_with_bootstrap_rpc_queue();
        let rejected_generation = advance_bootstrap_to_registration_request(&rejected_client);
        let rejected_scope = rejected_client.bootstrap_rpc_scope();
        let rejected_registration = rejected_scope.set_client_id(SetClientId {
            client_id: rejected_client.client_id.clone(),
            is_proxy: false,
        });
        let serial_before_rejection = rejected_client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);
        drop(rejected_receiver);
        let error = asupersync_block_on(rejected_registration)
            .expect_err("a closed real reader queue must reject registration enqueue");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                stage: RpcRetirementStage::Enqueue,
                certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                ..
            })
        ));
        assert_eq!(
            rejected_client
                .rpc_transport
                .lifecycle
                .lock()
                .protocol_for(rejected_generation)
                .expect("queue rejection leaves protocol authority live")
                .phase,
            RpcProtocolPhase::AwaitingRegistrationRequest
        );
        assert_eq!(
            rejected_client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before_rejection,
            "queue rejection must not allocate a serial"
        );

        let (client, receiver) = client_with_bootstrap_rpc_queue();
        let generation = advance_bootstrap_to_registration_request(&client);
        let scope = client.bootstrap_rpc_scope();
        let mut registration = Box::pin(scope.set_client_id(SetClientId {
            client_id: client.client_id.clone(),
            is_proxy: false,
        }));
        asupersync_block_on(poll_fn(|task_cx| {
            match registration.as_mut().poll(task_cx) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(result) => {
                    panic!(
                        "registration unexpectedly completed before reader dequeue: {:?}",
                        result
                    )
                }
            }
        }));
        assert_eq!(
            client
                .rpc_transport
                .lifecycle
                .lock()
                .protocol_for(generation)
                .expect("queued registration keeps protocol authority live")
                .phase,
            RpcProtocolPhase::AwaitingRegistrationResponse
        );
        drop(registration);

        let ReaderMessage::SendPdu { lease, promise, .. } = receiver
            .try_recv()
            .expect("registration must traverse the real reader queue")
        else {
            panic!("registration enqueued a non-PDU reader message");
        };
        assert!(promise.is_closed());
        assert!(
            lease.state.prepared.lock().is_none(),
            "pre-reader cancellation must drop the potentially large queued PDU immediately"
        );
        assert!(
            lease
                .claim_for_reader()
                .expect("canceled registration lease state should remain valid")
                .is_none(),
            "pre-reader cancellation must destroy the queued PDU owner"
        );
        drop(lease);
        assert_eq!(
            client
                .rpc_transport
                .lifecycle
                .lock()
                .protocol_for(generation)
                .expect("preclosed registration leaves protocol authority live")
                .phase,
            RpcProtocolPhase::AwaitingRegistrationRequest
        );

        let (response, ()) = asupersync_block_on(futures::future::join(
            scope.set_client_id(SetClientId {
                client_id: client.client_id.clone(),
                is_proxy: false,
            }),
            async {
                let ReaderMessage::SendPdu {
                    binding,
                    lease,
                    promise,
                } = receiver
                    .recv()
                    .await
                    .expect("a replacement registration must enqueue")
                else {
                    panic!("replacement registration enqueued a non-PDU message");
                };
                let prepared = lease
                    .claim_for_reader()
                    .expect("replacement registration reader claim should remain coherent")
                    .expect("replacement registration must retain its exact PDU");
                assert!(matches!(prepared.pdu(), Pdu::SetClientId(_)));
                let response = Pdu::UnitResponse(UnitResponse {});
                client
                    .rpc_transport
                    .complete_protocol_response(generation, binding.request, &response)
                    .expect("replacement registration establishes the protocol");
                promise
                    .send(Ok(PendingRpcReply::pdu(response)))
                    .await
                    .expect("replacement registration caller remains live");
            },
        ));
        assert_eq!(
            response.expect("replacement registration must complete"),
            UnitResponse {}
        );
        assert_eq!(
            client
                .rpc_transport
                .lifecycle
                .lock()
                .protocol_for(generation)
                .expect("replacement registration retains protocol authority")
                .phase,
            RpcProtocolPhase::Established
        );
    }

    #[test]
    fn readiness_cannot_publish_before_codec_and_registration_establish() {
        let (client, receiver) = client_with_bootstrap_rpc_queue();
        let rpc = client.bootstrap_rpc_scope();
        let mut guard = rpc
            .abort_guard("readiness-before-bootstrap test")
            .expect("register pending readiness participant");
        let error = asupersync_block_on(client.publish_rpc_transport_ready(&rpc, &guard))
            .expect_err("readiness must fail before bootstrap establishes protocol authority");
        assert!(error
            .to_string()
            .contains("before codec negotiation and client registration are established"));
        assert_eq!(
            client
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        guard.disarm();
    }

    #[test]
    fn registry_policy_matrix_covers_every_tuple_and_dialect_boundary() {
        let generation = NonZeroU64::new(13).expect("test generation is nonzero");
        let established = TopologyCapabilities::FENCED_SNAPSHOT_V1;
        let endpoint_tuples = [
            (PduProducer::Client, PduWireRole::Request),
            (PduProducer::Client, PduWireRole::CorrelatedReply),
            (PduProducer::Client, PduWireRole::Unilateral),
            (PduProducer::Server, PduWireRole::Request),
            (PduProducer::Server, PduWireRole::CorrelatedReply),
            (PduProducer::Server, PduWireRole::Unilateral),
        ];

        let mut closed_dialects = vec![LEGACY46_CODEC_VERSION];
        closed_dialects.extend(codec::CODEC_VERSION_MIN_SUPPORTED..=CODEC_VERSION);
        for spec in Pdu::all_wire_specs() {
            for agreed in closed_dialects.iter().copied() {
                let mut protocol = RpcProtocolAuthority::established_for_test(generation, agreed);
                protocol.established_capabilities = established;
                for (producer, role) in endpoint_tuples {
                    let direction = if producer == PduProducer::Client {
                        RpcProtocolDirection::Outbound
                    } else {
                        RpcProtocolDirection::Inbound
                    };
                    let capability_ok = match spec.capability {
                        PduCapabilityUse::None => true,
                        PduCapabilityUse::Negotiates(required) => {
                            RpcProtocolAuthority::locally_activated_capabilities()
                                .contains(required)
                        }
                        PduCapabilityUse::Requires(required) => established.contains(required),
                    };
                    let expected = spec.authorizes(producer, role)
                        && RpcProtocolAuthority::endpoint_is_activated(spec)
                        && protocol.wire_dialect().admits_wire_spec(spec)
                        && capability_ok;
                    assert_eq!(
                        protocol
                            .validate_common(spec, direction, producer, role)
                            .is_ok(),
                        expected,
                        "registry policy mismatch for {} ({}) at dialect {} and {:?}/{:?}",
                        spec.name,
                        spec.ident,
                        agreed,
                        producer,
                        role,
                    );
                }
            }
        }

        for gap in [5, 6, 7, 15, 16, 17, 18, 19, 21, u64::MAX] {
            assert_eq!(
                Pdu::wire_spec_for_ident(gap),
                None,
                "wire gap {gap} was assigned"
            );
        }
    }

    #[test]
    fn capability_policy_keeps_only_the_exact_topology_fence_active() {
        let generation = NonZeroU64::new(17).expect("test generation is nonzero");
        let mut protocol = RpcProtocolAuthority::established_for_test(generation, CODEC_VERSION);
        let coherent_request = Pdu::ListPanesCoherent(ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        });
        protocol
            .validate_outbound_pdu(&coherent_request, RpcOutboundAdmissionPoint::Preflight)
            .expect("the exact fenced snapshot offer remains active");

        let overbroad = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );
        assert!(matches!(
            protocol.validate_outbound_pdu(
                &Pdu::ListPanesCoherent(ListPanesCoherent {
                    supported: overbroad,
                    required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                }),
                RpcOutboundAdmissionPoint::Preflight,
            ),
            Err(OrdinaryMuxProtocolError::CapabilityAdvertisementMismatch { .. })
        ));

        assert!(matches!(
            protocol.validate_common(
                &<TopologyEvent as PduWireIdent>::WIRE_SPEC,
                RpcProtocolDirection::Inbound,
                PduProducer::Server,
                PduWireRole::Unilateral,
            ),
            Err(OrdinaryMuxProtocolError::CapabilityNotEstablished { .. })
        ));
        protocol
            .establish_capabilities(TopologyCapabilities::FENCED_SNAPSHOT_V1)
            .expect("successful coherent snapshot establishes only its fenced bit");
        protocol
            .validate_common(
                &<TopologyEvent as PduWireIdent>::WIRE_SPEC,
                RpcProtocolDirection::Inbound,
                PduProducer::Server,
                PduWireRole::Unilateral,
            )
            .expect("topology events activate only after the fenced snapshot");

        for inactive in [79, 80, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95] {
            let spec = Pdu::wire_spec_for_ident(inactive).expect("inactive PDU remains assigned");
            let authority = spec
                .authorities
                .first()
                .expect("assigned PDU has at least one authority");
            let direction = if authority.producer == PduProducer::Client {
                RpcProtocolDirection::Outbound
            } else {
                RpcProtocolDirection::Inbound
            };
            assert!(matches!(
                protocol.validate_common(spec, direction, authority.producer, authority.role),
                Err(OrdinaryMuxProtocolError::EndpointInactive { ident, .. }) if ident == inactive
            ));
        }

        for historical_gap in [5, 6, 7, 15, 16, 17, 18, 19, 21] {
            let synthetic = PduWireSpec {
                ident: historical_gap,
                name: "SyntheticFuturePdu",
                min_codec_version: CODEC_VERSION,
                producer: PduProducer::Client,
                capability: PduCapabilityUse::None,
                authorities: &[PduWireAuthority {
                    producer: PduProducer::Client,
                    role: PduWireRole::Request,
                }],
                encoded_body_limit: codec::PduEncodedBodyLimit::GlobalMaximum,
                semantic_class: codec::PduCorrelatedRequestPolicy::Fixed(
                    codec::PduSemanticClass::Query,
                ),
                admission_cap_key: codec::PduCorrelatedRequestPolicy::Fixed(
                    codec::PduAdmissionCapKey::Query,
                ),
                queue_qos: codec::PduCorrelatedRequestPolicy::Fixed(codec::PduQueueQos::Normal),
            };
            assert!(
                !RpcProtocolAuthority::endpoint_is_activated(&synthetic),
                "a future assignment in historical gap {} must default dormant",
                historical_gap
            );
        }
    }

    #[test]
    fn active_endpoint_identity_baseline_is_independent_of_policy_evaluation() {
        let mut expected = Vec::new();
        expected.extend(0..=4);
        expected.extend(8..=14);
        expected.push(20);
        expected.extend(22..=78);
        expected.extend(81..=83);
        expected.extend(96..=101);

        let actual = Pdu::all_wire_specs()
            .iter()
            .filter(|spec| RpcProtocolAuthority::endpoint_is_activated(spec))
            .map(|spec| spec.ident)
            .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "ordinary-client endpoint activation changed without updating its explicit baseline"
        );
    }

    #[test]
    fn production_reader_wire_serial_exhaustion_writes_zero_bytes_and_mints_no_successor() {
        let _watchdog = hang_watchdog(15, "wire serial exhaustion", 95);
        let (client, mut receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        client
            .rpc_transport
            .next_wire_serial
            .store(0, AtomicOrdering::Release);
        let state = ScriptedTransportState::new();
        let stream = ScriptedTransport {
            boundary: ScriptedFailureBoundary::AwaitingResponseEof,
            state: Arc::clone(&state),
        };
        let mut reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(UnixDomain::default()),
            Some(Box::new(stream)),
        );
        let request = client.send_pdu(Pdu::Ping(Ping {}));

        let (reader_result, request_result) = asupersync_block_on(futures::future::join(
            client_thread_async(&mut reconnectable, &mut receiver, &authority),
            request,
        ));
        let reader_error =
            reader_result.expect_err("wire serial exhaustion must terminate the reader");
        let caller_error =
            request_result.expect_err("wire serial exhaustion must complete the caller");
        for error in [&reader_error, &caller_error] {
            assert!(
                matches!(
                    error.downcast_ref::<RpcTransportError>(),
                    Some(RpcTransportError::WireSerialExhausted {
                        request: "Ping",
                        ..
                    })
                ),
                "wire serial exhaustion lost its typed classification: {:#}",
                error
            );
        }
        assert!(
            state.transcript().is_empty(),
            "an exhausted request must not be encoded or touch the wire"
        );
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION
        );

        let attempts_after_terminal = client
            .rpc_transport
            .next_attempt_id
            .load(AtomicOrdering::Acquire);
        let repeated_error = asupersync_block_on(client.send_pdu(Pdu::Ping(Ping {})))
            .expect_err("subsequent calls must retain the incarnation-terminal cause");
        assert!(matches!(
            repeated_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::WireSerialExhausted {
                request: "Ping",
                ..
            })
        ));
        assert_eq!(
            client
                .rpc_transport
                .next_attempt_id
                .load(AtomicOrdering::Acquire),
            attempts_after_terminal,
            "terminal calls must not consume another attempt identity"
        );

        let advance_error = match authority.advance_generation(&receiver) {
            Ok(_) => panic!("terminal identity exhaustion must not mint a successor"),
            Err(error) => error,
        };
        assert!(matches!(
            advance_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::WireSerialExhausted { .. })
        ));
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "terminal exhaustion must not publish G2"
        );
    }

    #[test]
    fn unpolled_attempt_identity_exhaustion_is_sticky_and_wakes_the_reader_once() {
        let (client, receiver) = client_with_idle_rpc_queue();
        client
            .rpc_transport
            .next_attempt_id
            .store(0, AtomicOrdering::Release);

        let unpolled = client.send_pdu(Pdu::Ping(Ping {}));
        drop(unpolled);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(matches!(
            client.rpc_transport.terminal_reader_wake_rx.try_recv(),
            Ok(())
        ));
        assert!(matches!(
            client.rpc_transport.terminal_reader_wake_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        for _ in 0..2 {
            let error = asupersync_block_on(client.send_pdu(Pdu::Ping(Ping {})))
                .expect_err("attempt identity exhaustion must fail before enqueue");
            assert!(matches!(
                error.downcast_ref::<RpcTransportError>(),
                Some(RpcTransportError::AttemptIdentityExhausted { request: "Ping" })
            ));
        }
        assert_eq!(
            client
                .rpc_transport
                .next_attempt_id
                .load(AtomicOrdering::Acquire),
            0
        );
        assert!(matches!(
            client.rpc_transport.terminal_error(),
            Some(RpcTransportError::AttemptIdentityExhausted { request: "Ping" })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn incarnation_terminal_cause_interrupts_a_blocked_reader_operation() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let blocked_transport = Arc::clone(&rpc_transport);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let blocked_reader = std::thread::spawn(move || {
            let operation = async move {
                started_tx
                    .send(())
                    .expect("announce polled reader operation");
                futures::future::pending::<()>().await;
            };
            let result = asupersync_block_on(blocked_transport.complete_before_terminal(operation));
            result_tx
                .send(result)
                .expect("publish blocked reader outcome");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader operation must start");

        let terminal =
            rpc_transport.mark_incarnation_terminal(RpcTransportError::AttemptIdentityExhausted {
                request: "Ping",
            });
        assert!(matches!(
            terminal,
            RpcTransportError::AttemptIdentityExhausted { request: "Ping" }
        ));
        let error = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("terminal cause must wake the blocked reader")
            .expect_err("a terminal wake must fail the blocked operation");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::AttemptIdentityExhausted { request: "Ping" })
        ));
        blocked_reader
            .join()
            .expect("blocked reader thread must not panic");

        let repeated =
            asupersync_block_on(rpc_transport.complete_before_terminal(futures::future::ready(())))
                .expect_err("the terminal cause must remain sticky after its wake is consumed");
        assert!(matches!(
            repeated.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::AttemptIdentityExhausted { request: "Ping" })
        ));
        assert!(matches!(
            rpc_transport.terminal_reader_wake_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn generation_abort_interrupts_a_blocked_reader_operation() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        let blocked_transport = Arc::clone(&rpc_transport);
        let blocked_abort = Arc::clone(&reader_abort);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let blocked_reader = std::thread::spawn(move || {
            let operation = async move {
                started_tx
                    .send(())
                    .expect("announce polled generation reader operation");
                futures::future::pending::<()>().await;
            };
            let result = asupersync_block_on(
                blocked_transport.complete_before_reader_stop(&blocked_abort, operation),
            );
            result_tx
                .send(result)
                .expect("publish generation-aborted reader outcome");
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader operation must start");

        assert!(rpc_transport.request_generation_abort(
            &reader_abort,
            "test cancellation while reader I/O is pending",
        ));
        let error = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("generation cancellation must wake the blocked reader")
            .expect_err("generation cancellation must fail the blocked operation");
        assert!(matches!(
            error.downcast_ref::<RpcGenerationReaderAborted>(),
            Some(RpcGenerationReaderAborted {
                generation: observed,
                reason: "test cancellation while reader I/O is pending",
            }) if *observed == generation
        ));
        assert_eq!(
            rpc_transport.live_generation.load(AtomicOrdering::Acquire),
            0,
            "cancellation must revoke admission before waking the reader"
        );
        blocked_reader
            .join()
            .expect("blocked generation reader thread must not panic");
    }

    #[test]
    fn sticky_generation_abort_wins_over_an_already_ready_operation() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        assert!(rpc_transport
            .request_generation_abort(&reader_abort, "ready-operation cancellation race",));

        let error =
            asupersync_block_on(rpc_transport.complete_before_reader_stop(
                &reader_abort,
                futures::future::ready("must-not-commit"),
            ))
            .expect_err("a committed cancellation must win over ready I/O");
        assert!(matches!(
            error.downcast_ref::<RpcGenerationReaderAborted>(),
            Some(RpcGenerationReaderAborted {
                generation: observed,
                reason: "ready-operation cancellation race",
            }) if *observed == generation
        ));
    }

    #[test]
    fn stale_generation_abort_authority_cannot_revoke_successor() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let first_generation = authority
            .rpc_transport
            .active_generation()
            .expect("first generation is live");
        let first_abort = authority
            .rpc_transport
            .reader_abort_for(first_generation)
            .expect("first generation has reader abort authority");

        let successor = authority
            .advance_generation(&receiver)
            .expect("retire first generation");
        successor
            .activate_rpc_transport()
            .expect("activate successor generation");
        authority
            .rpc_transport
            .mark_current_generation_ready_for_test();
        let successor_generation = authority
            .rpc_transport
            .active_generation()
            .expect("successor generation is live");
        let successor_abort = authority
            .rpc_transport
            .reader_abort_for(successor_generation)
            .expect("successor has fresh reader abort authority");

        assert!(!authority
            .rpc_transport
            .request_generation_abort(&first_abort, "stale first-generation cancellation",));
        assert!(first_abort.cause().is_none());
        assert!(successor_abort.cause().is_none());
        assert_eq!(
            authority
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            successor_generation.get()
        );
        assert_eq!(
            authority
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            successor_generation.get()
        );

        assert!(first_abort.commit_abort("queued stale wake"));
        first_abort.wake_reader();
        let successor_result =
            asupersync_block_on(authority.rpc_transport.complete_before_reader_stop(
                &successor_abort,
                futures::future::ready("successor remains live"),
            ))
            .expect("an old generation waker cannot affect its successor");
        assert_eq!(successor_result, "successor remains live");
    }

    #[test]
    fn first_generation_abort_reason_wins() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");

        assert!(rpc_transport.request_generation_abort(&reader_abort, "first cause"));
        assert!(!rpc_transport.request_generation_abort(&reader_abort, "second cause"));
        assert_eq!(reader_abort.cause(), Some("first cause"));
    }

    #[test]
    fn live_control_ack_rejects_abort_committed_after_reader_ack() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");

        assert!(
            rpc_transport.request_generation_abort(
                &reader_abort,
                "cancellation raced control acknowledgement",
            )
        );
        let error = rpc_transport
            .validate_live_control_ack(generation, &reader_abort, "test control operation")
            .expect_err("a revoked generation cannot publish a successful control result");
        assert!(matches!(
            error.downcast_ref::<RpcGenerationReaderAborted>(),
            Some(RpcGenerationReaderAborted {
                generation: observed,
                reason: "cancellation raced control acknowledgement",
            }) if *observed == generation
        ));
    }

    #[test]
    fn terminal_error_outranks_committed_generation_abort() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        assert!(rpc_transport
            .request_generation_abort(&reader_abort, "generation cancellation committed first",));
        rpc_transport.mark_incarnation_terminal(RpcTransportError::AttemptIdentityExhausted {
            request: "PublishReady",
        });

        let error = rpc_transport
            .validate_live_control_ack(generation, &reader_abort, "readiness publication")
            .expect_err("terminal authority must dominate a generation abort");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::AttemptIdentityExhausted {
                request: "PublishReady",
            })
        ));
    }

    #[test]
    fn terminal_error_outranks_generation_abort_at_reader_completion() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        assert!(rpc_transport
            .request_generation_abort(&reader_abort, "generation cancellation committed first",));
        rpc_transport.mark_incarnation_terminal(RpcTransportError::AttemptIdentityExhausted {
            request: "reader completion",
        });

        let error =
            asupersync_block_on(rpc_transport.complete_before_reader_stop(
                &reader_abort,
                futures::future::ready("must-not-commit"),
            ))
            .expect_err("terminal authority must dominate reader-generation cancellation");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::AttemptIdentityExhausted {
                request: "reader completion",
            })
        ));
    }

    #[test]
    fn terminal_commit_cannot_interleave_between_reader_stop_precedence_samples() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        let blocked_transport = Arc::clone(&rpc_transport);
        let blocked_abort = Arc::clone(&reader_abort);
        let terminal_transport = Arc::clone(&rpc_transport);
        let (reader_result_tx, reader_result_rx) = std::sync::mpsc::channel();
        let (terminal_result_tx, terminal_result_rx) = std::sync::mpsc::channel();

        // Hold the cause mutex after publishing a sticky cancellation. The
        // reader-stop sampler must keep the lifecycle lock while it blocks on
        // this cause; otherwise terminal authority can commit in the old gap
        // between the two precedence checks.
        let mut cause = reader_abort.cause.lock();
        *cause = Some("generation cause under precedence race");
        reader_abort.cancelled.store(true, AtomicOrdering::Release);
        let reader = std::thread::spawn(move || {
            let result = asupersync_block_on(blocked_transport.complete_before_reader_stop(
                &blocked_abort,
                futures::future::ready("must-not-commit"),
            ));
            reader_result_tx
                .send(result)
                .expect("publish reader-stop precedence result");
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while rpc_transport.lifecycle.try_lock().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "reader-stop sampler never acquired the lifecycle lock"
            );
            std::thread::yield_now();
        }
        let terminal = std::thread::spawn(move || {
            let error = terminal_transport.mark_incarnation_terminal(
                RpcTransportError::AttemptIdentityExhausted {
                    request: "precedence race",
                },
            );
            terminal_result_tx
                .send(error)
                .expect("publish terminal commit result");
        });
        assert!(matches!(
            terminal_result_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        drop(cause);
        let reader_error = reader_result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader-stop sample should complete after cause release")
            .expect_err("sticky generation cancellation must stop the reader");
        assert!(matches!(
            reader_error.downcast_ref::<RpcGenerationReaderAborted>(),
            Some(RpcGenerationReaderAborted {
                generation: observed,
                reason: "generation cause under precedence race",
            }) if *observed == generation
        ));
        assert!(matches!(
            terminal_result_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("terminal commit should proceed after precedence sampling"),
            RpcTransportError::AttemptIdentityExhausted {
                request: "precedence race",
            }
        ));
        reader
            .join()
            .expect("reader precedence thread must not panic");
        terminal
            .join()
            .expect("terminal precedence thread must not panic");
    }

    #[test]
    fn terminal_reader_authority_acquisition_preserves_terminal_cause() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        rpc_transport.mark_incarnation_terminal(RpcTransportError::AttemptIdentityExhausted {
            request: "reader startup",
        });

        for result in [
            rpc_transport.reader_abort_for(generation),
            rpc_transport.reader_abort_for_reader(generation),
        ] {
            let error = result.expect_err("terminal closure must reject reader authority");
            assert!(matches!(
                error.downcast_ref::<RpcTransportError>(),
                Some(RpcTransportError::AttemptIdentityExhausted {
                    request: "reader startup",
                })
            ));
        }
    }

    #[test]
    fn generation_abort_revokes_admission_but_retains_owning_reader_authority() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("test reader generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test reader has generation abort authority");
        assert!(rpc_transport
            .request_generation_abort(&reader_abort, "test exact-generation predicate split",));

        assert!(
            rpc_transport.reader_abort_for(generation).is_err(),
            "revoked admission must not mint an ordinary RPC scope"
        );
        let owning_reader = rpc_transport
            .reader_abort_for_reader(generation)
            .expect("the physical reader must recover its sticky wake authority");
        assert!(Arc::ptr_eq(&reader_abort, &owning_reader));
    }

    #[test]
    fn duplicate_readiness_participant_cancellation_hands_off_to_a_live_peer() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let authority = RpcReadinessAuthority::new(generation);
        assert!(authority
            .register_participant()
            .expect("register first readiness participant"));
        assert!(authority
            .register_participant()
            .expect("register duplicate readiness participant"));

        assert!(
            !authority.release_participant(true),
            "canceling one duplicate must transfer authority instead of aborting"
        );
        authority
            .mark_ready()
            .expect("the remaining participant must retain publication authority");
        assert!(
            !authority.release_participant(false),
            "successful participant release must not abort"
        );
        let state = authority.state.lock();
        assert_eq!(state.participants, 0);
        assert_eq!(state.phase, RpcReadinessAuthorityPhase::Ready);
    }

    #[test]
    fn last_readiness_participant_cancellation_blocks_late_publication() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let authority = RpcReadinessAuthority::new(generation);
        assert!(authority
            .register_participant()
            .expect("register readiness participant"));
        assert!(
            authority.release_participant(true),
            "the last cancelled participant must commit one abort"
        );
        let error = authority
            .mark_ready()
            .expect_err("publication cannot race past a committed last-participant abort");
        assert!(error
            .to_string()
            .contains("lost all readiness participants"));
        let error = authority
            .register_participant()
            .expect_err("a late participant cannot resurrect aborted authority");
        assert!(error
            .to_string()
            .contains("already committed readiness abort"));
    }

    #[test]
    fn readiness_participants_are_bounded_and_release_exactly() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let authority = RpcReadinessAuthority::new(generation);
        for _ in 0..MAX_RPC_READINESS_PARTICIPANTS {
            assert!(authority
                .register_participant()
                .expect("participant below the bound must register"));
        }
        let error = authority
            .register_participant()
            .expect_err("participant above the bound must be rejected");
        assert!(error.to_string().contains("readiness-participant limit"));

        for _ in 0..MAX_RPC_READINESS_PARTICIPANTS {
            assert!(!authority.release_participant(false));
        }
        let state = authority.state.lock();
        assert_eq!(state.participants, 0);
        assert_eq!(state.phase, RpcReadinessAuthorityPhase::Pending);
    }

    #[test]
    fn queued_readiness_publications_are_bounded_across_caller_cancellation() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let authority = Arc::new(RpcReadinessAuthority::new(generation));
        let (reader_sender, _reader_receiver) = unbounded();
        let mut queued = Vec::with_capacity(MAX_RPC_READINESS_PUBLICATIONS);

        for _ in 0..MAX_RPC_READINESS_PUBLICATIONS {
            let reservation = authority
                .reserve_publication()
                .expect("publication below the bound must reserve");
            let (promise, cancelled_result) = bounded(1);
            drop(cancelled_result);
            queued.push(ReaderMessage::PublishReady {
                generation,
                reader_sender: reader_sender.clone(),
                promise,
                reservation,
            });
        }
        assert_eq!(
            authority.state.lock().queued_publications,
            MAX_RPC_READINESS_PUBLICATIONS
        );
        let error = match authority.reserve_publication() {
            Ok(_) => panic!("publication above the bound must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("readiness-publication limit"));

        drop(
            queued
                .pop()
                .expect("one queued publication must be available to retire"),
        );
        let replacement = authority
            .reserve_publication()
            .expect("retiring one queued message must release one slot");
        assert_eq!(
            authority.state.lock().queued_publications,
            MAX_RPC_READINESS_PUBLICATIONS
        );
        drop(replacement);
        drop(queued);
        assert_eq!(authority.state.lock().queued_publications, 0);

        authority.retire();
        let error = match authority.reserve_publication() {
            Ok(_) => panic!("retired readiness authority must reject publication"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot accept"));
    }

    #[test]
    fn duplicate_readiness_guards_abort_only_after_the_last_participant_cancels() {
        let (scope, receiver, rpc_transport) = pending_readiness_scope_for_test();
        let reader_abort = scope
            .reader_abort
            .as_ref()
            .expect("pending scope has reader abort authority")
            .clone();
        let first = scope
            .abort_guard("first readiness participant cancelled")
            .expect("register first readiness participant");
        let second = scope
            .abort_guard("last readiness participant cancelled")
            .expect("register second readiness participant");

        drop(first);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert!(reader_abort.cause().is_none());
        drop(second);
        assert_eq!(
            reader_abort.cause(),
            Some("last readiness participant cancelled")
        );
        assert_eq!(
            rpc_transport.live_generation.load(AtomicOrdering::Acquire),
            0,
            "the last cancellation must revoke admission before waking the reader"
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn cancellation_after_readiness_commit_cannot_abort_the_generation() {
        let (scope, receiver, rpc_transport) = pending_readiness_scope_for_test();
        let guard = scope
            .abort_guard("readiness participant cancelled after commit")
            .expect("register readiness participant");
        let authority = Arc::clone(&rpc_transport.lifecycle.lock().readiness_authority);
        authority
            .mark_ready()
            .expect("live readiness participant authorizes commit");
        rpc_transport
            .ready_generation
            .store(INITIAL_CONNECTION_GENERATION, AtomicOrdering::Release);

        drop(guard);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        let state = authority.state.lock();
        assert_eq!(state.participants, 0);
        assert_eq!(state.phase, RpcReadinessAuthorityPhase::Ready);
    }

    #[test]
    fn fatal_replay_guard_commits_one_abort_despite_live_external_participant() {
        let (scope, receiver, rpc_transport) = pending_readiness_scope_for_test();
        let reader_abort = scope
            .reader_abort
            .as_ref()
            .expect("pending scope has reader abort authority")
            .clone();
        let mut external = scope
            .abort_guard("external readiness participant cancelled")
            .expect("register external readiness participant");
        let fatal = scope
            .fatal_abort_guard("pre-ready replay failed")
            .expect("register fatal replay guard");

        drop(fatal);
        assert_eq!(reader_abort.cause(), Some("pre-ready replay failed"));
        assert_eq!(
            rpc_transport.live_generation.load(AtomicOrdering::Acquire),
            0
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        let authority = Arc::clone(&rpc_transport.lifecycle.lock().readiness_authority);
        {
            let state = authority.state.lock();
            assert_eq!(state.participants, 1);
            assert_eq!(state.phase, RpcReadinessAuthorityPhase::AbortCommitted);
        }
        assert!(
            authority.register_participant().is_err(),
            "fatal abort must prevent participant resurrection"
        );

        external.disarm();
        drop(external);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        let state = authority.state.lock();
        assert_eq!(state.participants, 0);
        assert_eq!(state.phase, RpcReadinessAuthorityPhase::AbortCommitted);
    }

    #[test]
    fn panicking_readiness_leader_hands_authority_to_a_live_participant() {
        let (scope, receiver, rpc_transport) = pending_readiness_scope_for_test();
        let leader = scope
            .abort_guard("panicking readiness leader")
            .expect("register readiness leader");
        let mut follower = scope
            .abort_guard("live readiness follower")
            .expect("register readiness follower");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _leader = leader;
            panic!("simulated readiness leader panic");
        }));
        assert!(panic.is_err());
        assert!(
            matches!(receiver.try_recv(), Err(async_channel::TryRecvError::Empty)),
            "leader panic must not abort while another participant is live"
        );

        let authority = Arc::clone(&rpc_transport.lifecycle.lock().readiness_authority);
        authority
            .mark_ready()
            .expect("the live follower must retain readiness authority");
        follower.disarm();
        drop(follower);
        let state = authority.state.lock();
        assert_eq!(state.participants, 0);
        assert_eq!(state.phase, RpcReadinessAuthorityPhase::Ready);
    }

    #[test]
    fn readiness_coordinator_coalesces_duplicates_before_and_during_replay() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let mut queue = PreReadyUnilateralQueue::default();
        for (window_id, title) in [(1, "first"), (2, "second")] {
            queue
                .enqueue(
                    unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                        window_id,
                        title: title.to_string(),
                    })),
                    0,
                    0,
                )
                .expect("queue deterministic pre-ready unilateral");
        }

        let mut readiness = RpcReadinessCoordinator::default();
        let (leader, leader_result) = bounded(1);
        let (before, before_result) = bounded(1);
        readiness.admit(leader);
        readiness.admit(before);

        let (first_pdus, first_bytes) = match readiness
            .next_action(generation, &mut queue)
            .expect("start the one elected replay")
        {
            RpcReadinessNextAction::StartReplay {
                batch,
                replayed_bytes,
            } => (batch.len(), replayed_bytes),
            _ => panic!("the first publication must elect one replay"),
        };
        assert_eq!(first_pdus, 1);
        assert_eq!(readiness.replayed_in_flight(), (first_pdus, first_bytes));

        drop(leader_result);
        let (during, during_result) = bounded(1);
        readiness.admit(during);
        assert!(matches!(
            readiness
                .next_action(generation, &mut queue)
                .expect("coalesce a publication during replay"),
            RpcReadinessNextAction::AwaitInFlightReplay
        ));
        readiness
            .finish_replay(generation, generation, first_pdus, first_bytes)
            .expect("finish the elected replay with exact accounting");

        let (second_pdus, second_bytes) = match readiness
            .next_action(generation, &mut queue)
            .expect("start the next quarantined replay batch")
        {
            RpcReadinessNextAction::StartReplay {
                batch,
                replayed_bytes,
            } => (batch.len(), replayed_bytes),
            _ => panic!("queued unilateral work must replay before readiness"),
        };
        readiness
            .finish_replay(generation, generation, second_pdus, second_bytes)
            .expect("finish the second replay with exact accounting");
        assert!(matches!(
            readiness
                .next_action(generation, &mut queue)
                .expect("commit only after every pre-ready obligation"),
            RpcReadinessNextAction::CommitReady
        ));

        readiness.complete_success();
        before_result
            .try_recv()
            .expect("duplicate-before-replay waiter must complete")
            .expect("duplicate-before-replay waiter must share success");
        during_result
            .try_recv()
            .expect("duplicate-during-replay waiter must complete")
            .expect("duplicate-during-replay waiter must share success");
        assert!(readiness.waiters.waiting.is_empty());
    }

    #[test]
    fn retired_readiness_replay_completion_cannot_consume_successor_accounting() {
        let retired_generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let successor_generation = NonZeroU64::new(
            retired_generation
                .get()
                .checked_add(1)
                .expect("test generation has a successor"),
        )
        .expect("successor generation is nonzero");
        let mut queue = PreReadyUnilateralQueue::default();
        queue
            .enqueue(
                unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 1,
                    title: "successor replay".to_string(),
                })),
                0,
                0,
            )
            .expect("queue successor pre-ready unilateral");

        let mut readiness = RpcReadinessCoordinator::default();
        let (successor_pdus, successor_bytes) = match readiness
            .next_action(successor_generation, &mut queue)
            .expect("start successor replay")
        {
            RpcReadinessNextAction::StartReplay {
                batch,
                replayed_bytes,
            } => (batch.len(), replayed_bytes),
            _ => panic!("queued successor work must start replay"),
        };

        assert_eq!(
            readiness
                .finish_replay(
                    successor_generation,
                    retired_generation,
                    successor_pdus,
                    successor_bytes,
                )
                .expect("retired completion must be ignored by the successor"),
            RpcReadinessReplayCompletion::RetiredGeneration
        );
        assert_eq!(
            readiness.replayed_in_flight(),
            (successor_pdus, successor_bytes),
            "retired completion must not consume successor replay authority"
        );
        let future_generation = NonZeroU64::new(
            successor_generation
                .get()
                .checked_add(1)
                .expect("successor test generation has a future"),
        )
        .expect("future generation is nonzero");
        readiness
            .finish_replay(
                successor_generation,
                future_generation,
                successor_pdus,
                successor_bytes,
            )
            .expect_err("a future-generation completion must fail closed");
        assert_eq!(
            readiness.replayed_in_flight(),
            (successor_pdus, successor_bytes),
            "future completion must not consume successor replay authority"
        );
        assert_eq!(
            readiness
                .finish_replay(
                    successor_generation,
                    successor_generation,
                    successor_pdus,
                    successor_bytes,
                )
                .expect("successor completion must retain exact accounting"),
            RpcReadinessReplayCompletion::CurrentGeneration
        );
        assert_eq!(readiness.replayed_in_flight(), (0, 0));
    }

    #[test]
    fn readiness_replay_failure_fans_out_one_terminal_result() {
        let generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("generation is nonzero");
        let mut queue = PreReadyUnilateralQueue::default();
        queue
            .enqueue(
                unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 1,
                    title: "failing replay".to_string(),
                })),
                0,
                0,
            )
            .expect("queue deterministic failing replay input");
        let mut readiness = RpcReadinessCoordinator::default();
        let (first, first_result) = bounded(1);
        let (second, second_result) = bounded(1);
        readiness.admit(first);
        readiness.admit(second);
        let (replayed_pdus, replayed_bytes) = match readiness
            .next_action(generation, &mut queue)
            .expect("start replay")
        {
            RpcReadinessNextAction::StartReplay {
                batch,
                replayed_bytes,
            } => (batch.len(), replayed_bytes),
            _ => panic!("queued work must start replay"),
        };
        readiness
            .finish_replay(generation, generation, replayed_pdus, replayed_bytes)
            .expect("failed replay still must report exact accounting");
        let terminal = "deterministic pre-ready replay failure";
        readiness.complete_error(terminal);

        let first = first_result
            .try_recv()
            .expect("first waiter must complete")
            .expect_err("first waiter must observe replay failure");
        let second = second_result
            .try_recv()
            .expect("second waiter must complete")
            .expect_err("second waiter must observe replay failure");
        assert_eq!(first.to_string(), terminal);
        assert_eq!(second.to_string(), terminal);
        assert!(readiness.waiters.waiting.is_empty());
    }

    #[test]
    fn readiness_waiters_are_bounded_and_complete_independently() {
        let mut waiters = RpcReadinessWaiters::default();
        let (cancelled_tx, cancelled_rx) = bounded(1);
        drop(cancelled_rx);
        waiters.admit(cancelled_tx);
        assert!(waiters.waiting.is_empty());

        let mut receivers = Vec::with_capacity(MAX_RPC_READINESS_WAITERS);
        for _ in 0..MAX_RPC_READINESS_WAITERS {
            let (waiter, receiver) = bounded(1);
            waiters.admit(waiter);
            receivers.push(receiver);
        }
        assert_eq!(waiters.waiting.len(), MAX_RPC_READINESS_WAITERS);

        let (rejected, rejected_result) = bounded(1);
        waiters.admit(rejected);
        let error = rejected_result
            .try_recv()
            .expect("over-limit waiter must receive a result")
            .expect_err("over-limit waiter must be rejected");
        assert!(error.to_string().contains("coalesced-waiter limit"));

        waiters.complete_success();
        assert!(waiters.waiting.is_empty());
        for receiver in receivers {
            receiver
                .try_recv()
                .expect("every admitted waiter must complete")
                .expect("every admitted waiter must observe the same success");
        }

        let (full, full_result) = bounded(1);
        full.try_send(Ok(()))
            .expect("pre-fill the completion channel");
        waiters.admit(full);
        let (live_after_full, live_after_full_result) = bounded(1);
        waiters.admit(live_after_full);
        waiters.complete_success();
        full_result
            .try_recv()
            .expect("the pre-filled completion remains available")
            .expect("pre-filled completion is successful");
        live_after_full_result
            .try_recv()
            .expect("a full leader channel must not strand another waiter")
            .expect("the live waiter must still receive readiness success");
        assert!(waiters.waiting.is_empty());
    }

    #[test]
    fn retired_readiness_publication_cannot_cross_successor_activation() {
        let (sender, receiver) = unbounded();
        let rpc_transport = Arc::new(RpcTransportState::new());
        let client = Client {
            sender: sender.clone(),
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport: Arc::clone(&rpc_transport),
            domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
            client_id: ClientId::new(),
            client_domain_config: ClientDomainConfig::Unix(UnixDomain::default()),
            is_reconnectable: false,
            is_local: true,
        };
        let stale = client.test_dispatch_authority(Weak::new());
        let stale_scope = client.bootstrap_rpc_scope();
        let mut stale_guard = stale_scope
            .abort_guard("stale readiness publisher")
            .expect("register first-generation readiness participant");
        let stale_authority = Arc::clone(&rpc_transport.lifecycle.lock().readiness_authority);
        let reservation = stale_authority
            .reserve_publication()
            .expect("reserve first-generation readiness publication");
        let (publication, publication_result) = bounded(1);
        sender
            .try_send(ReaderMessage::PublishReady {
                generation: NonZeroU64::new(INITIAL_CONNECTION_GENERATION)
                    .expect("initial generation is nonzero"),
                reader_sender: sender.clone(),
                promise: publication,
                reservation,
            })
            .expect("queue first-generation readiness publication");

        let successor = stale
            .advance_generation(&receiver)
            .expect("retire the first generation and mint its successor");
        let retired = publication_result
            .try_recv()
            .expect("retirement must complete the queued publication")
            .expect_err("a retired readiness publication must fail");
        assert!(retired
            .to_string()
            .contains("retired before reader admission"));
        assert_eq!(stale_authority.state.lock().queued_publications, 0);
        successor
            .activate_rpc_transport()
            .expect("activate the exact successor generation");

        let stale_error = commit_rpc_transport_ready(
            &stale,
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("initial generation is nonzero"),
        )
        .expect_err("the retired generation cannot publish readiness into its successor");
        assert!(stale_error.to_string().contains("retired"));
        assert_eq!(
            rpc_transport.ready_generation.load(AtomicOrdering::Acquire),
            0
        );

        let successor_scope = client.bootstrap_rpc_scope();
        let mut successor_guard = successor_scope
            .abort_guard("successor readiness publisher")
            .expect("register successor readiness participant");
        let successor_generation =
            NonZeroU64::new(successor.generation).expect("successor generation is nonzero");
        commit_rpc_transport_ready(&successor, successor_generation)
            .expect("only the successor may publish its readiness");
        assert_eq!(
            rpc_transport.ready_generation.load(AtomicOrdering::Acquire),
            successor.generation
        );

        stale_guard.disarm();
        successor_guard.disarm();
    }

    #[test]
    fn connection_generation_exhaustion_closes_without_publishing_a_successor() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let mut authority = client.test_dispatch_authority(Weak::new());
        let maximum = NonZeroU64::MAX;
        authority.generation = maximum.get();
        authority
            .connection_generation
            .store(maximum.get(), AtomicOrdering::Release);
        {
            let mut lifecycle = authority.rpc_transport.lifecycle.lock();
            lifecycle.phase = RpcTransportPhase::Live(maximum);
        }
        authority
            .rpc_transport
            .live_generation
            .store(maximum.get(), AtomicOrdering::Release);
        authority
            .rpc_transport
            .ready_generation
            .store(maximum.get(), AtomicOrdering::Release);

        let error = match authority.advance_generation(&receiver) {
            Ok(_) => panic!("generation exhaustion must not mint a successor"),
            Err(error) => error,
        };
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::ConnectionGenerationExhausted {
                last_generation
            }) if *last_generation == maximum
        ));
        assert!(matches!(
            authority.rpc_transport.terminal_error(),
            Some(RpcTransportError::ConnectionGenerationExhausted {
                last_generation
            }) if last_generation == maximum
        ));
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            maximum.get(),
            "terminal exhaustion must leave the final published generation intact"
        );
        assert_eq!(
            authority
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            0
        );
        assert_eq!(
            authority
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0
        );
        assert!(receiver.is_closed());
    }

    #[test]
    fn client_outbound_budget_reserves_capacity_for_small_key_input() {
        let current_dialect = MuxWireDialect::current(CODEC_VERSION)
            .expect("the build codec version is a closed current dialect");
        let prepare_normal = || {
            Pdu::ListPanes(ListPanes {})
                .prepare_outbound_for_dialect(
                    current_dialect,
                    PduProducer::Client,
                    PduWireRole::Request,
                    None,
                    CompressionMode::Auto,
                )
                .expect("state-sync request should plan")
        };
        let normal = prepare_normal();
        let key = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'k'],
        })
        .prepare_outbound_for_dialect(
            current_dialect,
            PduProducer::Client,
            PduWireRole::Request,
            None,
            CompressionMode::Auto,
        )
        .expect("one-byte key input should plan");
        assert_eq!(normal.metadata().queue_qos, PduQueueQos::Normal);
        assert_eq!(key.metadata().queue_qos, PduQueueQos::Interactive);
        let normal_codec_bytes = normal.codec_peak_bytes();

        let total_codec_bytes = normal_codec_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(key.codec_peak_bytes()))
            .expect("small test plans should not overflow");
        let budget = Arc::new(ClientOutboundBudget::with_limits(
            ClientOutboundBudgetLimits {
                total_codec_bytes,
                noninteractive_codec_bytes: normal_codec_bytes,
                total_slots: 3,
                noninteractive_slots: 1,
            },
        ));
        let generation = NonZeroU64::new(1).expect("test generation is nonzero");
        let normal_lease = budget
            .try_reserve(Weak::new(), generation, normal)
            .expect("first noninteractive request should fill its lane");
        let rejected = budget
            .try_reserve(Weak::new(), generation, prepare_normal())
            .expect_err("a second noninteractive request must not borrow the reserve");
        assert_eq!(
            rejected.limit,
            ClientOutboundBudgetLimit::NoninteractiveCodecBytes
        );
        let key_lease = budget
            .try_reserve(Weak::new(), generation, key)
            .expect("small key input must retain reserved admission capacity");
        let saturated = budget.snapshot();
        assert_eq!(saturated.slots, 2);
        assert_eq!(saturated.noninteractive_slots, 1);
        assert_eq!(saturated.noninteractive_codec_bytes, normal_codec_bytes);

        drop(normal_lease);
        drop(key_lease);
        let released = budget.snapshot();
        assert_eq!(released.codec_bytes, 0);
        assert_eq!(released.noninteractive_codec_bytes, 0);
        assert_eq!(released.slots, 0);
        assert_eq!(released.noninteractive_slots, 0);
        assert_eq!(released.peak_codec_bytes, saturated.codec_bytes);
    }

    #[test]
    fn client_outbound_rejects_plan_before_serial_queue_or_codec_work() {
        let ping = Pdu::Ping(Ping {})
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("ping should produce a reference plan");
        let rejected_limit = ping
            .codec_peak_bytes()
            .checked_sub(1)
            .expect("a ping plan should retain nonzero codec bytes");
        let (client, receiver) =
            client_with_idle_rpc_queue_and_limits(ClientOutboundBudgetLimits {
                total_codec_bytes: rejected_limit,
                noninteractive_codec_bytes: rejected_limit,
                total_slots: 1,
                noninteractive_slots: 1,
            });
        let serial_before = client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);

        let error = asupersync_block_on(client.ping())
            .expect_err("a request above its aggregate byte envelope must fail closed");
        assert!(matches!(
            error.downcast_ref::<ClientOutboundAdmissionError>(),
            Some(ClientOutboundAdmissionError {
                ident: Ping::IDENT,
                planned_codec_bytes,
                limit: ClientOutboundBudgetLimit::TotalCodecBytes,
            }) if *planned_codec_bytes == ping.codec_peak_bytes()
        ));
        assert_eq!(
            error
                .downcast_ref::<ClientOutboundAdmissionError>()
                .expect("budget rejection should remain typed")
                .delivery_certainty(),
            RpcDeliveryCertainty::DefinitelyNotSent
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert_eq!(
            client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before
        );
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot(),
            ClientOutboundBudgetState::default()
        );
    }

    #[test]
    fn unpolled_and_canceled_client_requests_release_one_exact_lease() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let serial_before = client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);

        let unpolled = client.ping();
        let reserved = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(reserved.slots, 1);
        assert!(reserved.codec_bytes > 0);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        drop(unpolled);
        let released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(released.slots, 0);
        assert_eq!(released.codec_bytes, 0);

        let canceled = admit_interactive_rpc_now(client.ping())
            .expect("live request should enter the reader queue")
            .expect("queued request should await a response");
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 1);
        drop(canceled);
        let canceled_payload_released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(canceled_payload_released.slots, 1);
        assert_eq!(canceled_payload_released.codec_bytes, 0);
        let message = receiver
            .try_recv()
            .expect("canceled request remains exactly once in the reader queue");
        let ReaderMessage::SendPdu { promise, lease, .. } = &message else {
            panic!("canceled request enqueued a non-PDU message");
        };
        assert!(promise.is_closed());
        assert!(
            lease.state.prepared.lock().is_none(),
            "pre-reader cancellation must drop the potentially large queued PDU immediately"
        );
        assert!(
            lease
                .claim_for_reader()
                .expect("canceled reader claim state should remain coherent")
                .is_none(),
            "a pre-reader caller cancellation must revoke encoding authority"
        );
        drop(message);
        let canceled_fully_released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(canceled_fully_released.slots, 0);
        assert_eq!(canceled_fully_released.codec_bytes, 0);
        assert_eq!(
            canceled_fully_released.peak_codec_bytes, canceled_payload_released.peak_codec_bytes,
            "dropping the stale queue shell must not settle its byte lease twice"
        );
        assert_eq!(
            client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before,
            "unpolled and pre-reader-canceled requests must not consume a serial"
        );
    }

    #[test]
    fn canceled_queue_shells_remain_count_bounded_after_payload_release() {
        let ping_plan = Pdu::Ping(Ping {})
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("ping should produce a reference plan");
        let (client, receiver) =
            client_with_idle_rpc_queue_and_limits(ClientOutboundBudgetLimits {
                total_codec_bytes: ping_plan
                    .codec_peak_bytes()
                    .checked_mul(3)
                    .expect("three ping plans fit in the test byte budget"),
                noninteractive_codec_bytes: ping_plan
                    .codec_peak_bytes()
                    .checked_mul(3)
                    .expect("three ping plans fit in the noninteractive test budget"),
                total_slots: 2,
                noninteractive_slots: 2,
            });
        let first = admit_interactive_rpc_now(client.ping())
            .expect("first ping should enter the queue")
            .expect("first ping should await a response");
        let second = admit_interactive_rpc_now(client.ping())
            .expect("second ping should enter the queue")
            .expect("second ping should await a response");
        drop(first);
        drop(second);
        let canceled = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(canceled.codec_bytes, 0);
        assert_eq!(canceled.slots, 2);

        let error = asupersync_block_on(client.ping())
            .expect_err("canceled queue shells must continue to consume the count bound");
        assert!(matches!(
            error.downcast_ref::<ClientOutboundAdmissionError>(),
            Some(ClientOutboundAdmissionError {
                limit: ClientOutboundBudgetLimit::TotalSlots,
                ..
            })
        ));
        drop(
            receiver
                .try_recv()
                .expect("first canceled queue shell should remain bounded and drainable"),
        );
        drop(
            receiver
                .try_recv()
                .expect("second canceled queue shell should remain bounded and drainable"),
        );
        let drained = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(drained.codec_bytes, 0);
        assert_eq!(drained.slots, 0);
    }

    #[test]
    fn reader_claim_retains_outbound_budget_after_caller_abandonment() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let waiter = admit_interactive_rpc_now(client.ping())
            .expect("live request should enter the reader queue")
            .expect("queued request should await a response");
        let ReaderMessage::SendPdu {
            binding,
            lease,
            promise,
        } = receiver
            .try_recv()
            .expect("the reader should receive the admitted request")
        else {
            panic!("admitted request enqueued a non-PDU message");
        };
        let prepared = lease
            .claim_for_reader()
            .expect("active reader claim state should remain coherent")
            .expect("active reader claim must retain the exact PDU");
        drop(waiter);
        assert!(promise.is_closed());
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot().slots,
            1,
            "caller abandonment must not release bytes owned by the active reader"
        );

        client
            .rpc_transport
            .rollback_unadmitted_outbound(binding.generation, prepared.pdu())
            .expect("test reader claim should roll back its protocol transition");
        drop(prepared);
        drop(promise);
        drop(lease);
        let released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(released.slots, 0);
        assert_eq!(released.codec_bytes, 0);
    }

    #[test]
    fn interactive_rpc_admission_enqueues_now_and_awaits_without_blocking() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let pending = admit_interactive_rpc_now(client.ping())
            .expect("live interactive RPC admission should succeed")
            .expect("an admitted RPC should await its reader response");

        let message = receiver
            .try_recv()
            .expect("interactive admission must enqueue during the caller's turn");
        let ReaderMessage::SendPdu { lease, promise, .. } = message else {
            panic!("interactive admission enqueued a non-RPC reader message");
        };
        let prepared = lease
            .claim_for_reader()
            .expect("interactive reader claim state should remain coherent")
            .expect("interactive reader claim must retain the exact PDU");
        assert!(matches!(prepared.pdu(), Pdu::Ping(Ping {})));
        promise
            .try_send(Ok(PendingRpcReply::pdu(Pdu::Pong(Pong {}))))
            .expect("complete admitted interactive RPC");
        asupersync_block_on(pending).expect("admitted interactive RPC should observe its reply");
    }

    #[test]
    fn delivered_rpc_reply_survives_retirement_before_caller_observation() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let pending = admit_interactive_rpc_now(client.ping())
            .expect("live RPC admission should succeed")
            .expect("admitted RPC should await its reader response");

        let ReaderMessage::SendPdu { lease, promise, .. } = receiver
            .try_recv()
            .expect("the physical reader should receive the admitted RPC")
        else {
            panic!("admitted RPC enqueued a non-PDU reader message");
        };
        let prepared = lease
            .claim_for_reader()
            .expect("reader claim state should remain coherent")
            .expect("the live reader should retain the exact request");
        assert!(matches!(prepared.pdu(), Pdu::Ping(Ping {})));

        promise
            .try_send(Ok(PendingRpcReply::pdu(Pdu::Pong(Pong {}))))
            .expect("the validated response should enter the one-shot channel");
        authority
            .begin_rpc_transport_retirement()
            .expect("EOF after response delivery should retire the old transport");
        assert_eq!(
            client
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            0,
            "the response must be observed after its transport is no longer live"
        );

        let response = asupersync_block_on(pending)
            .expect("retirement after delivery must not erase a correlated response");
        assert_eq!(response, Pong {});
        drop(prepared);
        drop(lease);
    }

    #[test]
    fn interactive_rpc_admission_reports_a_closed_queue_immediately() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let serial_before = client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);
        let request = client.ping();
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 1);
        drop(receiver);

        let error = match admit_interactive_rpc_now(request) {
            Err(error) => error,
            Ok(_) => panic!("closed reader queue must reject interactive input immediately"),
        };
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                stage: RpcRetirementStage::Enqueue,
                certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                ..
            })
        ));
        let released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(released.slots, 0);
        assert_eq!(released.codec_bytes, 0);
        assert_eq!(
            client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before,
            "queue rejection must precede serial assignment"
        );
    }

    #[test]
    fn full_reader_queue_releases_only_the_rejected_request_lease() {
        let (client, receiver) = client_with_bounded_idle_rpc_queue(1);
        let serial_before = client
            .rpc_transport
            .next_wire_serial
            .load(AtomicOrdering::Acquire);
        let first = admit_interactive_rpc_now(client.ping())
            .expect("first request should fill the one-slot queue")
            .expect("first request should await a response");
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 1);

        let error = match admit_interactive_rpc_now(client.ping()) {
            Err(error) => error,
            Ok(_) => panic!("second request must fail while the physical queue is full"),
        };
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                stage: RpcRetirementStage::Enqueue,
                certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                ..
            })
        ));
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot().slots,
            1,
            "queue failure must release the rejected lease without touching the incumbent"
        );
        assert_eq!(
            client
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            serial_before
        );

        drop(first);
        drop(
            receiver
                .try_recv()
                .expect("the incumbent request should remain queued exactly once"),
        );
        let released = client.rpc_transport.outbound_budget.snapshot();
        assert_eq!(released.slots, 0);
        assert_eq!(released.codec_bytes, 0);
    }

    #[test]
    fn rpc_future_binds_synchronously_but_never_enqueues_before_first_poll() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let first_generation_scope = client.rpc_scope();

        let bound_on_first = client.send_pdu(Pdu::Ping(Ping {}));
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot().slots,
            1,
            "synchronous planning must retain one exact unpolled lease"
        );
        assert!(
            matches!(receiver.try_recv(), Err(async_channel::TryRecvError::Empty)),
            "constructing a bound RPC future must retain normal lazy-future semantics"
        );

        let successor = authority
            .advance_generation(&receiver)
            .expect("retire the generation captured by the unpolled future");
        let error = asupersync_block_on(bound_on_first)
            .expect_err("a future first-polled after reconnect must fail before enqueue");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                bound_generation,
                active_generation: None,
                stage: RpcRetirementStage::Enqueue,
                certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                ..
            }) if bound_generation.get() == INITIAL_CONNECTION_GENERATION
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 0);
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot().codec_bytes,
            0
        );

        let during_reconnect = client.send_pdu(Pdu::Ping(Ping {}));
        let error = asupersync_block_on(during_reconnect)
            .expect_err("external admission during reconnect must fail immediately");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Unavailable {
                stage: RpcRetirementStage::Admission,
                ..
            })
        ));

        successor
            .activate_rpc_transport()
            .expect("publish the exact successor generation");
        let stale_scoped_call = first_generation_scope.ping();
        let error = asupersync_block_on(stale_scoped_call)
            .expect_err("a reusable first-generation scope must never auto-upgrade");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                bound_generation,
                active_generation: Some(active_generation),
                stage: RpcRetirementStage::Admission,
                certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                ..
            }) if bound_generation.get() == INITIAL_CONNECTION_GENERATION
                && active_generation.get() == successor.generation
        ));
        let before_successor_handshake = client.send_pdu(Pdu::WriteToPane(WriteToPane {
            pane_id: 9,
            data: b"must-not-overtake-handshake".to_vec(),
        }));
        let error = asupersync_block_on(before_successor_handshake)
            .expect_err("ambient effectful RPC must not enter an unready successor");
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Unavailable {
                request: "WriteToPane",
                stage: RpcRetirementStage::Admission,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        client
            .rpc_transport
            .mark_current_generation_ready_for_test();
        let fresh_but_unpolled = client.send_pdu(Pdu::Ping(Ping {}));
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
        drop(fresh_but_unpolled);
        assert_eq!(client.rpc_transport.outbound_budget.snapshot().slots, 0);
        assert_eq!(
            client.rpc_transport.outbound_budget.snapshot().codec_bytes,
            0
        );
    }

    #[test]
    fn render_connection_identity_is_bound_to_one_exact_live_rpc_generation() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let first_scope = client.rpc_scope();
        let first_generation =
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("test generation is nonzero");
        let successor_identity = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x36; 16]),
            MuxSessionIncarnation::from_bytes([0x58; 16]),
        );

        assert_eq!(first_scope.render_connection_identity(), None);
        let reserved_identity = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0; 16]),
            MuxSessionIncarnation::from_bytes([0; 16]),
        );
        assert!(
            client
                .rpc_transport
                .bind_render_connection_identity(first_generation, reserved_identity)
                .is_err(),
            "a reserved wire default must never acquire render authority"
        );
        assert_eq!(first_scope.render_connection_identity(), None);

        client
            .rpc_transport
            .bind_render_connection_identity(first_generation, TEST_RENDER_CONNECTION_IDENTITY)
            .expect("the coherent first-generation snapshot should bind its identity");
        client
            .rpc_transport
            .bind_render_connection_identity(first_generation, TEST_RENDER_CONNECTION_IDENTITY)
            .expect("replaying the exact committed snapshot should be idempotent");
        assert_eq!(
            first_scope.render_connection_identity(),
            Some(TEST_RENDER_CONNECTION_IDENTITY)
        );
        assert!(
            client
                .rpc_transport
                .bind_render_connection_identity(first_generation, successor_identity)
                .is_err(),
            "one live RPC generation must never replace its established identity"
        );

        let successor = authority
            .advance_generation(&receiver)
            .expect("retire the first render generation");
        assert_eq!(
            first_scope.render_connection_identity(),
            None,
            "retirement must immediately revoke stale render authority"
        );
        successor
            .activate_rpc_transport()
            .expect("activate the exact successor RPC generation");
        let successor_scope = client.bootstrap_rpc_scope();
        assert_eq!(
            successor_scope.render_connection_identity(),
            None,
            "a successor must remain render-ineligible until its coherent snapshot commits"
        );
        assert!(
            client
                .rpc_transport
                .bind_render_connection_identity(first_generation, TEST_RENDER_CONNECTION_IDENTITY,)
                .is_err(),
            "a stale generation must not bind after successor publication"
        );

        let successor_generation = NonZeroU64::new(successor.generation)
            .expect("successor generation should remain nonzero");
        client
            .rpc_transport
            .bind_render_connection_identity(successor_generation, successor_identity)
            .expect("the successor coherent snapshot should establish new render authority");
        assert_eq!(
            successor_scope.render_connection_identity(),
            Some(successor_identity)
        );
        client
            .rpc_transport
            .mark_current_generation_ready_for_test();
        let ready_successor_scope = client.rpc_scope();
        assert_eq!(
            ready_successor_scope.render_connection_identity(),
            Some(successor_identity),
            "readiness publication must preserve the exact bootstrapped identity"
        );
        assert_eq!(
            first_scope.render_connection_identity(),
            None,
            "a stale scope must never observe its successor's identity"
        );

        successor.close_rpc_transport_without_receiver();
        assert_eq!(
            successor_scope.render_connection_identity(),
            None,
            "connection close must revoke the final render identity"
        );
        assert_eq!(ready_successor_scope.render_connection_identity(), None);
    }

    #[test]
    fn consumer_commit_lease_drains_before_successor_publication_and_rejects_stale_commits() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let first_scope = client.rpc_scope();
        let nested_scope = first_scope.clone();
        let stale_scope = first_scope.clone();
        let (commit_entered_tx, commit_entered_rx) = std::sync::mpsc::channel();
        let (release_commit_tx, release_commit_rx) = std::sync::mpsc::channel();

        let commit_thread = std::thread::spawn(move || {
            first_scope.commit_sync(RpcConsumerKind::TopologySnapshot, || {
                nested_scope
                    .commit_sync(RpcConsumerKind::Search, || ())
                    .expect("consumer commits must be reentrant without holding the gate mutex");
                commit_entered_tx
                    .send(())
                    .expect("announce admitted consumer commit");
                release_commit_rx
                    .recv()
                    .expect("release admitted consumer commit");
                42_u64
            })
        });
        commit_entered_rx
            .recv()
            .expect("the first-generation consumer commit must start");

        let retiring_authority = authority.clone();
        let retiring_receiver = receiver.clone();
        let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::channel();
        let (successor_tx, successor_rx) = std::sync::mpsc::channel();
        let retirement_thread = std::thread::spawn(move || {
            retiring_authority
                .begin_rpc_transport_retirement()
                .expect("close new consumer-commit admission");
            retirement_started_tx
                .send(())
                .expect("announce transport retirement");
            let successor = retiring_authority
                .advance_generation(&retiring_receiver)
                .map(|authority| authority.generation);
            successor_tx
                .send(successor)
                .expect("publish successor result");
        });
        retirement_started_rx
            .recv()
            .expect("retirement must close admission");

        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "G2 must remain unpublished while a G1 consumer lease is active"
        );
        assert!(matches!(
            successor_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        let stale_error = client
            .rpc_scope()
            .commit_sync(RpcConsumerKind::Search, || {
                panic!("an ambient commit captured during retirement must never execute")
            })
            .expect_err("retirement must reject new consumer commits");
        assert!(matches!(
            stale_error,
            RpcConsumerCommitError::Unavailable {
                consumer: RpcConsumerKind::Search
            }
        ));
        let stale_error = stale_scope
            .commit_sync(RpcConsumerKind::Search, || {
                panic!("an exact stale-generation commit must never execute")
            })
            .expect_err("retirement must reject an already-captured generation");
        assert!(matches!(
            stale_error,
            RpcConsumerCommitError::Retired {
                consumer: RpcConsumerKind::Search,
                bound_generation,
                active_generation: None,
            } if bound_generation.get() == INITIAL_CONNECTION_GENERATION
        ));

        release_commit_tx
            .send(())
            .expect("release first-generation consumer");
        assert_eq!(
            commit_thread
                .join()
                .expect("consumer thread must not panic")
                .expect("admitted consumer commit must complete"),
            42
        );
        let successor_generation = successor_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("successor publication must unblock after lease drop")
            .expect("successor generation must be created");
        retirement_thread
            .join()
            .expect("retirement thread must not panic");
        assert_eq!(successor_generation, INITIAL_CONNECTION_GENERATION + 1);
        assert_eq!(
            authority
                .connection_generation
                .load(AtomicOrdering::Acquire),
            successor_generation
        );
    }

    #[test]
    fn successor_publication_divergence_is_terminal_and_wakes_the_reader() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let first_scope = client.rpc_scope();
        let (commit_entered_tx, commit_entered_rx) = std::sync::mpsc::channel();
        let (release_commit_tx, release_commit_rx) = std::sync::mpsc::channel();
        let commit_thread = std::thread::spawn(move || {
            first_scope
                .commit_sync(RpcConsumerKind::TopologySnapshot, || {
                    commit_entered_tx
                        .send(())
                        .expect("announce admitted consumer commit");
                    release_commit_rx
                        .recv_timeout(Duration::from_secs(1))
                        .expect("release admitted consumer commit");
                })
                .expect("first-generation consumer commit must be admitted");
        });
        commit_entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("consumer commit must start");

        let retiring_authority = authority.clone();
        let retiring_receiver = receiver.clone();
        let (retirement_started_tx, retirement_started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let retirement_thread = std::thread::spawn(move || {
            retiring_authority
                .begin_rpc_transport_retirement()
                .expect("close first-generation admission");
            retirement_started_tx
                .send(())
                .expect("announce transport retirement");
            result_tx
                .send(retiring_authority.advance_generation(&retiring_receiver))
                .expect("publish successor result");
        });
        retirement_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("transport retirement must start");

        let divergent_generation = INITIAL_CONNECTION_GENERATION + 99;
        authority
            .connection_generation
            .store(divergent_generation, AtomicOrdering::Release);
        release_commit_tx
            .send(())
            .expect("release first-generation consumer");

        let successor_result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("divergent successor publication must terminate");
        let error = match successor_result {
            Ok(_) => panic!("generation divergence must not mint a successor"),
            Err(error) => error,
        };
        assert!(matches!(
            error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::ConnectionGenerationDiverged {
                retiring_generation,
                expected_generation,
                observed_generation,
            }) if retiring_generation.get() == INITIAL_CONNECTION_GENERATION
                && expected_generation.get() == INITIAL_CONNECTION_GENERATION
                && *observed_generation == divergent_generation
        ));
        assert!(matches!(
            authority.rpc_transport.terminal_error(),
            Some(RpcTransportError::ConnectionGenerationDiverged {
                observed_generation,
                ..
            }) if observed_generation == divergent_generation
        ));
        assert!(receiver.is_closed());
        assert!(matches!(
            authority.rpc_transport.terminal_reader_wake_rx.try_recv(),
            Ok(())
        ));
        commit_thread
            .join()
            .expect("consumer commit thread must not panic");
        retirement_thread
            .join()
            .expect("retirement thread must not panic");
    }

    #[test]
    fn consumer_commit_lease_drop_is_panic_safe() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let rpc = client.rpc_scope();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = rpc.commit_sync(RpcConsumerKind::PaneUnilateral, || {
                panic!("scripted consumer panic");
            });
        }));
        assert!(panic.is_err());
        assert_eq!(
            client
                .rpc_transport
                .lifecycle
                .lock()
                .active_consumer_commits,
            0,
            "RAII drop must release the consumer lease during unwinding"
        );
        authority
            .advance_generation(&receiver)
            .expect("panic-safe lease release must not strand retirement");
    }

    #[test]
    fn transport_retirement_drains_queued_generation_with_one_typed_outcome() {
        let (client, receiver) = client_with_idle_rpc_queue();
        let authority = client.test_dispatch_authority(Weak::new());
        let requests = vec![
            Pdu::WriteToPane(WriteToPane {
                pane_id: 11,
                data: b"keypress".to_vec(),
            }),
            Pdu::SpawnV2(SpawnV2 {
                domain: config::keyassignment::SpawnTabDomain::default(),
                window_id: None,
                command: None,
                command_dir: None,
                size: wezterm_term::TerminalSize::default(),
                workspace: "campaign".to_string(),
            }),
            Pdu::SplitPane(SplitPane {
                pane_id: 12,
                split_request: mux::tab::SplitRequest::default(),
                command: None,
                command_dir: None,
                domain: config::keyassignment::SpawnTabDomain::default(),
                move_pane_id: None,
            }),
            Pdu::MovePaneToNewTab(MovePaneToNewTab {
                pane_id: 13,
                window_id: None,
                workspace_for_new_window: Some("campaign".to_string()),
            }),
            Pdu::KillPane(KillPane { pane_id: 14 }),
            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id: 15,
                pattern: mux::pane::Pattern::CaseSensitiveString("needle".to_string()),
                range: 0..100,
                limit: Some(8),
            }),
            Pdu::Resize(Resize {
                containing_tab_id: 16,
                pane_id: 17,
                size: wezterm_term::TerminalSize::default(),
            }),
            Pdu::Ping(Ping {}),
        ];
        let mut results = Vec::with_capacity(requests.len());
        asupersync_block_on(async {
            for pdu in requests {
                let request = pdu.pdu_name();
                let (completion, result) = bounded(1);
                client
                    .sender
                    .send(client.test_reader_message(pdu, completion))
                    .await
                    .expect("queue an exact-generation request");
                results.push((request, result));
            }
        });

        authority
            .advance_generation(&receiver)
            .expect("retire and drain the queued generation");
        for (request, result) in results {
            let error = result
                .try_recv()
                .expect("retirement must complete the queued request")
                .expect_err("queued work must not cross onto the successor");
            assert!(matches!(
                error.downcast_ref::<RpcTransportError>(),
                Some(RpcTransportError::Retired {
                    request: observed_request,
                    stage: RpcRetirementStage::Queued,
                    certainty: RpcDeliveryCertainty::DefinitelyNotSent,
                    active_generation: None,
                    ..
                }) if *observed_request == request
            ));
            assert!(matches!(
                result.try_recv(),
                Err(async_channel::TryRecvError::Empty) | Err(async_channel::TryRecvError::Closed)
            ));
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }

    #[test]
    fn wire_serials_are_never_reused_across_transport_generations() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let connection_generation = Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION));
        let authority = ClientDispatchAuthority::new(
            None,
            Weak::new(),
            Arc::new(ClientIncarnation),
            Arc::clone(&connection_generation),
            Arc::clone(&rpc_transport),
        );
        let (first_metrics, first_probe) = RpcMetricProbe::new();
        let mut first = PendingReplies::new(
            first_metrics,
            NonZeroU64::new(INITIAL_CONNECTION_GENERATION).expect("initial generation is nonzero"),
            Arc::clone(&rpc_transport),
        );
        let (first_tx, first_rx) = bounded(1);
        let first_serial = first
            .admit_named(first_tx, "Ping")
            .expect("admit first-generation request")
            .expect("assign first-generation serial");
        assert_eq!(first_serial.get(), 1);
        first
            .set_stage(first_serial, RpcRetirementStage::AwaitingResponse)
            .expect("track the first request as emitted");
        first.fail_all("first transport disconnected");
        assert!(first_rx
            .try_recv()
            .expect("first waiter must retire")
            .is_err());
        first_probe.assert_balanced();

        let (_sender, receiver) = unbounded();
        let successor = authority
            .advance_generation(&receiver)
            .expect("mint successor generation");
        successor
            .activate_rpc_transport()
            .expect("activate successor generation");
        let successor_generation =
            NonZeroU64::new(successor.generation).expect("successor generation is nonzero");
        let (second_metrics, second_probe) = RpcMetricProbe::new();
        let mut second = PendingReplies::new(
            second_metrics,
            successor_generation,
            Arc::clone(&rpc_transport),
        );
        let (second_tx, second_rx) = bounded(1);
        let second_serial = second
            .admit_named(second_tx, "Ping")
            .expect("admit successor request")
            .expect("assign successor serial");
        assert_eq!(second_serial.get(), 2);

        let stale = second
            .complete(first_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect_err("an old-generation serial must not match successor state");
        assert!(matches!(
            stale,
            PendingRpcError::UnmatchedSerial { serial, .. } if serial == first_serial
        ));
        assert!(matches!(
            second_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));

        second
            .complete(second_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect("the exact successor serial should complete");
        assert!(matches!(
            second_rx
                .try_recv()
                .expect("successor completion")
                .expect("successor RPC result"),
            PendingRpcReply::Pdu(pdu) if matches!(pdu.as_ref(), Pdu::Pong(Pong {}))
        ));
        second_probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_gauge_aggregates_across_connections_and_drop_drains_exactly_once() {
        let (metrics, probe) = RpcMetricProbe::new();
        let mut first = pending_replies_with_metrics(metrics.clone());
        let (first_tx, first_rx) = bounded(1);
        let first_serial = first
            .admit_named(first_tx, "Ping")
            .expect("admit first connection RPC")
            .expect("assign first connection serial");
        assert_eq!(probe.pending(), 1.0);

        let mut second = pending_replies_with_metrics(metrics);
        assert_eq!(
            probe.pending(),
            1.0,
            "constructing another connection must not reset the process gauge"
        );
        let (second_tx, second_rx) = bounded(1);
        second
            .admit_named(second_tx, "SearchScrollbackRequest")
            .expect("admit second connection RPC")
            .expect("assign second connection serial");
        assert_eq!(probe.pending(), 2.0);

        first
            .complete(first_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect("first connection reply should enqueue");
        assert!(first_rx
            .try_recv()
            .expect("first connection completion")
            .is_ok());
        assert_eq!(probe.pending(), 1.0);

        drop(second);
        assert!(second_rx
            .try_recv()
            .expect("PendingReplies::drop must wake the live waiter")
            .is_err());
        assert_eq!(probe.pending(), 0.0);
        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 1);
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_preclosed_admission_consumes_neither_frame_slot_nor_serial() {
        let (mut pending, probe) = pending_replies_for_test();
        let (preclosed_tx, preclosed_rx) = bounded(1);
        drop(preclosed_rx);

        assert_eq!(
            pending
                .admit_named(preclosed_tx, "SearchScrollbackRequest")
                .expect("preclosed admission is a normal disposition"),
            None
        );
        assert_eq!(pending.highest_issued(), 0);
        assert!(pending.map.is_empty());
        assert_eq!(RpcMetricProbe::counter(&probe.preclosed), 1);
        probe.assert_balanced();

        let (live_tx, live_rx) = bounded(1);
        let serial = pending
            .admit_named(live_tx, "Ping")
            .expect("live request should admit")
            .expect("live request should receive a serial");
        assert_eq!(serial.get(), 1);
        assert_eq!(pending.highest_issued(), 1);
        assert_eq!(probe.pending(), 1.0);

        assert_eq!(
            pending
                .complete(serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
                .expect("live response should deliver")
                .disposition,
            ReplyDisposition::Delivered
        );
        assert!(matches!(
            live_rx
                .try_recv()
                .expect("live response should be queued")
                .expect("live response should be successful"),
            PendingRpcReply::Pdu(pdu) if matches!(pdu.as_ref(), Pdu::Pong(Pong {}))
        ));
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_max_serial_is_issued_once_then_exhaustion_is_typed_and_terminal() {
        let (mut pending, probe) = pending_replies_for_test();
        pending
            .rpc_transport
            .next_wire_serial
            .store(u64::MAX, AtomicOrdering::Release);
        pending.highest_issued = u64::MAX - 1;

        let (max_tx, max_rx) = bounded(1);
        let max_serial = pending
            .admit_named(max_tx, "Ping")
            .expect("maximum serial should be admissible once")
            .expect("maximum serial should be assigned");
        assert_eq!(max_serial.get(), u64::MAX);
        assert_eq!(
            pending
                .rpc_transport
                .next_wire_serial
                .load(AtomicOrdering::Acquire),
            0
        );

        let (exhausted_tx, exhausted_rx) = bounded(1);
        let exhausted = pending
            .admit_named(exhausted_tx, "Ping")
            .expect_err("the serial after u64::MAX must fail closed");
        assert!(matches!(
            exhausted,
            PendingRpcError::IncarnationTerminal(RpcTransportError::WireSerialExhausted {
                request: "Ping",
                ..
            })
        ));
        let caller_error = exhausted_rx
            .try_recv()
            .expect("admission failure should wake the caller")
            .expect_err("serial exhaustion must be delivered as a typed error");
        assert!(matches!(
            caller_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::WireSerialExhausted {
                request: "Ping",
                ..
            })
        ));
        assert_eq!(RpcMetricProbe::counter(&probe.serial_exhausted), 1);
        assert!(matches!(
            pending.rpc_transport.terminal_error(),
            Some(RpcTransportError::WireSerialExhausted {
                request: "Ping",
                ..
            })
        ));
        assert_eq!(
            pending
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            0,
            "serial exhaustion must permanently close RPC admission"
        );

        pending.fail_all("wire serial space exhausted");
        assert!(max_rx
            .try_recv()
            .expect("the maximum-serial waiter must retire exactly once")
            .is_err());
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_serial_collision_never_replaces_the_original_waiter() {
        let (mut pending, probe) = pending_replies_for_test();
        let (original_tx, original_rx) = bounded(1);
        let original_serial = pending
            .admit_named(original_tx, "Ping")
            .expect("admit original request")
            .expect("assign original serial");
        pending
            .rpc_transport
            .next_wire_serial
            .store(original_serial.get(), AtomicOrdering::Release);

        let (collision_tx, collision_rx) = bounded(1);
        let collision = pending
            .admit_named(collision_tx, "SearchScrollbackRequest")
            .expect_err("occupied serial must not be replaced");
        assert!(matches!(
            collision,
            PendingRpcError::SerialCollision {
                serial,
                request: "SearchScrollbackRequest",
                pending_request: "Ping",
            } if serial == original_serial
        ));
        assert!(collision_rx
            .try_recv()
            .expect("collision should wake the caller")
            .is_err());
        assert_eq!(pending.map.len(), 1);
        assert_eq!(
            pending
                .map
                .get(&original_serial)
                .expect("original pending request must remain")
                .binding
                .request,
            "Ping"
        );
        assert_eq!(RpcMetricProbe::counter(&probe.serial_collision), 1);

        pending
            .complete(original_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect("the original request should complete");
        assert!(original_rx.try_recv().expect("original response").is_ok());
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_completion_classifies_delivery_abandonment_and_protocol_errors() {
        let (mut pending, probe) = pending_replies_for_test();

        let (delivered_tx, delivered_rx) = bounded(1);
        let delivered_serial = pending
            .admit_named(delivered_tx, "Ping")
            .expect("admit delivered request")
            .expect("assign delivered serial");
        assert_eq!(
            pending
                .complete(delivered_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})),)
                .expect("response before receiver drop should deliver")
                .disposition,
            ReplyDisposition::Delivered
        );
        drop(delivered_rx);

        let duplicate = pending
            .complete(delivered_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect_err("a duplicate response must be fatal");
        assert!(matches!(
            duplicate,
            PendingRpcError::UnmatchedSerial { serial, .. } if serial == delivered_serial
        ));

        let future_serial =
            NonZeroU64::new(delivered_serial.get() + 1).expect("future serial is nonzero");
        let future = pending
            .complete(future_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect_err("a never-issued future response must be fatal");
        assert!(matches!(
            future,
            PendingRpcError::FutureSerial { serial, .. } if serial == future_serial
        ));

        let (abandoned_tx, abandoned_rx) = bounded(1);
        let abandoned_serial = pending
            .admit_named(abandoned_tx, "SearchScrollbackRequest")
            .expect("admit abandoned request")
            .expect("assign abandoned serial");
        drop(abandoned_rx);
        assert_eq!(
            pending
                .complete(
                    abandoned_serial,
                    PendingRpcReply::pdu(Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    },)),
                )
                .expect("a late response to an abandoned caller must drain")
                .disposition,
            ReplyDisposition::Abandoned
        );

        let (full_tx, full_rx) = bounded(1);
        let filler = full_tx.clone();
        let full_serial = pending
            .admit_named(full_tx, "Ping")
            .expect("admit full-channel request")
            .expect("assign full-channel serial");
        filler
            .try_send(Ok(PendingRpcReply::pdu(Pdu::Pong(Pong {}))))
            .expect("test should prefill completion channel");
        let full = pending
            .complete(full_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect_err("a full one-shot reply channel is an invariant failure");
        assert!(matches!(
            full,
            PendingRpcError::ReplyChannelFull { serial, .. } if serial == full_serial
        ));
        drop(full_rx);

        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.unmatched_serial), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.future_serial), 1);
        assert_eq!(
            RpcMetricProbe::counter(&probe.retirement_reply_channel_full),
            1
        );
        assert_eq!(
            RpcMetricProbe::counter(&probe.protocol_reply_channel_full),
            1
        );
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn inactive_ordered_window_family_is_rejected_from_every_header_shape_before_body() {
        let rpc_transport = RpcTransportState::new();
        rpc_transport.mark_current_generation_ready_for_test();
        let generation = rpc_transport
            .active_generation()
            .expect("ordered-window test transport starts live");
        let ordered_idents = [
            <ListPanesOrderedV1 as PduWireIdent>::IDENT,
            <ListPanesOrderedV1Response as PduWireIdent>::IDENT,
            <ReorderWindowTabsV1 as PduWireIdent>::IDENT,
            <ReorderWindowTabsV1Response as PduWireIdent>::IDENT,
            <WindowOrderEventV1 as PduWireIdent>::IDENT,
        ];
        assert_eq!(ordered_idents, [86, 87, 88, 89, 90]);

        let payload = [0xa5; 257];
        for ident in ordered_idents {
            for serial in [0, 1, 127, u64::MAX] {
                for compressed in [false, true] {
                    let (wire, header_len) = test_opaque_frame(ident, serial, compressed, &payload);
                    let mut reader = std::io::Cursor::new(wire);
                    let mut downstream_selector_reached = false;
                    let error = asupersync_block_on(Pdu::decode_async_with_selector(
                        &mut reader,
                        None,
                        |header| {
                            validate_ordinary_mux_inbound_header(
                                &rpc_transport,
                                generation,
                                header,
                                u64::MAX,
                            )?;
                            downstream_selector_reached = true;
                            Ok(PduBodyDisposition::Materialize)
                        },
                    ))
                    .expect_err("inactive ordered-window header must fail closed");

                    assert!(
                        !downstream_selector_reached,
                        "ordered-window rejection must precede downstream correlation"
                    );
                    assert_eq!(
                        usize::try_from(reader.position()).expect("cursor position fits usize"),
                        header_len,
                        "ordered-window rejection must leave the complete body unread"
                    );
                    assert_eq!(
                        error.downcast_ref::<NotReconnectableError>(),
                        Some(&NotReconnectableError::InactiveOrderedWindowPdu {
                            ident,
                            serial,
                            encoded_payload_len: payload.len(),
                            compressed,
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn additive_render_families_use_registry_inactive_errors_not_ordered_classification() {
        let rpc_transport = RpcTransportState::new();
        rpc_transport.mark_current_generation_ready_for_test();
        let generation = rpc_transport
            .active_generation()
            .expect("additive-family test transport starts live");
        let payload = [0x5a; 31];
        for (ident, serial) in [
            (<RenderApplicationUpdateV1 as PduWireIdent>::IDENT, 0),
            (<RenderApplicationUpdate as PduWireIdent>::IDENT, 0),
            (<GetPaneRenderDeliveryV1Response as PduWireIdent>::IDENT, 7),
        ] {
            for compressed in [false, true] {
                let (wire, header_len) = test_opaque_frame(ident, serial, compressed, &payload);
                let mut reader = std::io::Cursor::new(wire);
                let mut downstream_selector_reached = false;
                let error = asupersync_block_on(Pdu::decode_async_with_selector(
                    &mut reader,
                    None,
                    |header| {
                        validate_ordinary_mux_inbound_header(
                            &rpc_transport,
                            generation,
                            header,
                            u64::MAX,
                        )?;
                        downstream_selector_reached = true;
                        Err(anyhow!("inactive additive PDU reached downstream selector"))
                    },
                ))
                .expect_err("inactive additive PDU must fail before its opaque body");

                assert!(!downstream_selector_reached);
                assert!(matches!(
                    error.downcast_ref::<NotReconnectableError>(),
                    Some(NotReconnectableError::ProtocolViolation(
                        OrdinaryMuxProtocolError::EndpointInactive {
                            ident: observed_ident,
                            ..
                        }
                    )) if *observed_ident == ident
                ));
                assert_eq!(
                    usize::try_from(reader.position()).expect("cursor position fits usize"),
                    header_len,
                    "registry inactive rejection must leave its body unread"
                );
            }
        }
    }

    #[test]
    fn bootstrap_rejects_unilateral_headers_before_registration_and_body_admission() {
        let rpc_transport = RpcTransportState::new();
        let generation = rpc_transport
            .active_generation()
            .expect("bootstrap unilateral test transport starts live");
        let payload = [0x3b; 257];

        for phase in [
            RpcProtocolPhase::AwaitingRegistrationRequest,
            RpcProtocolPhase::AwaitingRegistrationResponse,
        ] {
            let mut protocol =
                RpcProtocolAuthority::established_for_test(generation, CODEC_VERSION);
            protocol.phase = phase;
            rpc_transport.lifecycle.lock().protocol = Some(protocol);

            for compressed in [false, true] {
                let (wire, header_len) = test_opaque_frame(
                    <NotifyAlert as PduWireIdent>::IDENT,
                    0,
                    compressed,
                    &payload,
                );
                let mut reader = std::io::Cursor::new(wire);
                let mut downstream_selector_reached = false;
                let error = asupersync_block_on(Pdu::decode_async_with_selector(
                    &mut reader,
                    None,
                    |header| {
                        validate_ordinary_mux_inbound_header(
                            &rpc_transport,
                            generation,
                            header,
                            u64::MAX,
                        )?;
                        downstream_selector_reached = true;
                        Ok(PduBodyDisposition::Materialize)
                    },
                ))
                .expect_err("pre-registration unilateral header must fail closed");

                assert!(!downstream_selector_reached);
                assert!(matches!(
                    error.downcast_ref::<NotReconnectableError>(),
                    Some(NotReconnectableError::ProtocolViolation(
                        OrdinaryMuxProtocolError::PhaseViolation {
                            phase: observed_phase,
                            ..
                        }
                    )) if *observed_phase == phase
                ));
                assert_eq!(
                    usize::try_from(reader.position()).expect("cursor position fits usize"),
                    header_len,
                    "bootstrap phase rejection must leave the complete body unread",
                );
            }
        }
    }

    #[test]
    fn inbound_header_role_and_pending_authority_precede_every_response_body() {
        let rpc_transport = RpcTransportState::new();
        rpc_transport.mark_current_generation_ready_for_test();
        let generation = rpc_transport
            .active_generation()
            .expect("header-role test transport starts live");
        // Header-role admission must be the rejecting authority in this test.
        // Use the smallest schema cap in the table at its exact legal boundary
        // so encoded-body admission cannot make the negative controls vacuous.
        let payload = [0x6d; codec::MAX_MUX_ERROR_RESPONSE_DECOMPRESSED_BYTES];

        for (ident, serial, authorized) in [
            (
                <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
                0,
                true,
            ),
            (
                <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
                1,
                true,
            ),
            (<ErrorResponse as PduWireIdent>::IDENT, 0, false),
            (<ErrorResponse as PduWireIdent>::IDENT, 1, true),
            (<Pong as PduWireIdent>::IDENT, 0, false),
            (<PaneRemoved as PduWireIdent>::IDENT, 1, false),
        ] {
            let (wire, header_len) = test_opaque_frame(ident, serial, false, &payload);
            let mut reader = std::io::Cursor::new(wire);
            let mut downstream_selector_reached = false;
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                None,
                |header| {
                    validate_ordinary_mux_inbound_header(
                        &rpc_transport,
                        generation,
                        header,
                        u64::MAX,
                    )?;
                    downstream_selector_reached = true;
                    Err(anyhow!("authorized header reached pending correlation"))
                },
            ))
            .expect_err("the selector deliberately leaves every opaque body unread");

            assert_eq!(
                downstream_selector_reached, authorized,
                "ident {ident}, serial {serial}"
            );
            if !authorized {
                assert!(
                    matches!(
                        error.downcast_ref::<NotReconnectableError>(),
                        Some(NotReconnectableError::ProtocolViolation(
                            OrdinaryMuxProtocolError::DirectionViolation { .. }
                        ))
                    ),
                    "ident {}, serial {} produced unexpected rejection: {:#}",
                    ident,
                    serial,
                    error
                );
            }
            assert_eq!(
                usize::try_from(reader.position()).expect("cursor position fits usize"),
                header_len,
                "role admission must never read the response body"
            );
        }

        let synthetic_agreed = LEGACY46_CODEC_VERSION;
        rpc_transport.lifecycle.lock().protocol = Some(RpcProtocolAuthority::established_for_test(
            generation,
            synthetic_agreed,
        ));
        for (ident, unknown) in [
            (5, true),
            (<ListPanesCoherentResponse as PduWireIdent>::IDENT, false),
        ] {
            let (wire, header_len) = test_opaque_frame(ident, 1, true, &payload);
            let mut reader = std::io::Cursor::new(wire);
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                None,
                |header| {
                    validate_ordinary_mux_inbound_header(
                        &rpc_transport,
                        generation,
                        header,
                        u64::MAX,
                    )?;
                    panic!("unknown or above-dialect header must fail before body admission");
                },
            ))
            .expect_err("unknown and above-dialect inbound identities fail closed");
            let protocol_error = match error.downcast_ref::<NotReconnectableError>() {
                Some(NotReconnectableError::ProtocolViolation(error)) => error,
                other => panic!(
                    "unexpected protocol rejection for ident {}: {:?}",
                    ident, other
                ),
            };
            if unknown {
                assert!(matches!(
                    protocol_error,
                    OrdinaryMuxProtocolError::UnknownPdu { ident: 5, .. }
                ));
            } else {
                assert!(matches!(
                    protocol_error,
                    OrdinaryMuxProtocolError::DialectViolation {
                        ident: observed_ident,
                        agreed,
                        ..
                    } if *observed_ident == ident
                        && *agreed == synthetic_agreed
                ));
            }
            assert_eq!(
                usize::try_from(reader.position()).expect("cursor position fits usize"),
                header_len,
                "unknown and above-dialect headers must leave bodies unread"
            );
        }

        for ident in [
            <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
            <ErrorResponse as PduWireIdent>::IDENT,
        ] {
            let (mut pending, probe) = pending_replies_for_test();
            pending.rpc_transport.lifecycle.lock().protocol = Some(
                RpcProtocolAuthority::established_for_test(pending.generation, CODEC_VERSION),
            );
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named_expect(
                    completion_tx,
                    "GetPaneRenderChanges",
                    NonZeroU64::new(<GetPaneRenderChangesResponse as PduWireIdent>::IDENT),
                )
                .expect("admit dual-role correlation probe")
                .expect("assign dual-role correlation serial");
            pending
                .set_stage(serial, RpcRetirementStage::AwaitingResponse)
                .expect("mark dual-role probe awaiting response");
            let generation = pending.generation;
            let rpc_transport = Arc::clone(&pending.rpc_transport);
            let (wire, header_len) = test_opaque_frame(ident, serial.get(), false, &payload);
            let mut reader = std::io::Cursor::new(wire);
            let mut correlation_reached = false;
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                None,
                |header| {
                    validate_ordinary_mux_inbound_header(
                        &rpc_transport,
                        generation,
                        header,
                        pending.highest_issued(),
                    )?;
                    let disposition = pending.response_body_disposition(serial, header)?;
                    assert!(matches!(
                        disposition,
                        PendingResponseBodyDisposition::Materialize(_)
                    ));
                    correlation_reached = true;
                    Err(anyhow!("authorized correlation reached body admission"))
                },
            ))
            .expect_err("the correlation selector deliberately stops before the body");
            assert!(
                correlation_reached,
                "ident {} must reach exact pending authority",
                ident
            );
            assert_eq!(
                pending.map[&serial].stage,
                RpcRetirementStage::ResponseMatch
            );
            assert_eq!(
                usize::try_from(reader.position()).expect("cursor position fits usize"),
                header_len
            );
            pending.fail_after_decode_error(&error);
            assert!(completion_rx
                .try_recv()
                .expect("terminal cleanup wakes the pending caller exactly once")
                .is_err());
            assert!(matches!(
                completion_rx.try_recv(),
                Err(async_channel::TryRecvError::Empty) | Err(async_channel::TryRecvError::Closed)
            ));
            probe.assert_balanced();
        }

        let (mut pending, probe) = pending_replies_for_test();
        pending.rpc_transport.lifecycle.lock().protocol = Some(
            RpcProtocolAuthority::established_for_test(pending.generation, CODEC_VERSION),
        );
        let (completion_tx, completion_rx) = bounded(1);
        let serial = pending
            .admit_named_expect(completion_tx, "Ping", Some(test_wire_ident::<Pong>()))
            .expect("admit unilateral-on-pending probe")
            .expect("assign unilateral-on-pending serial");
        pending
            .set_stage(serial, RpcRetirementStage::AwaitingResponse)
            .expect("mark unilateral-on-pending probe awaiting response");
        let generation = pending.generation;
        let rpc_transport = Arc::clone(&pending.rpc_transport);
        let (wire, header_len) = test_opaque_frame(
            <PaneRemoved as PduWireIdent>::IDENT,
            serial.get(),
            false,
            &payload,
        );
        let mut reader = std::io::Cursor::new(wire);
        let error = asupersync_block_on(Pdu::decode_async_with_selector(
            &mut reader,
            None,
            |header| {
                validate_ordinary_mux_inbound_header(
                    &rpc_transport,
                    generation,
                    header,
                    pending.highest_issued(),
                )?;
                panic!("unsolicited-shaped PDU must fail before pending correlation");
            },
        ))
        .expect_err("unilateral-shaped PDU on a pending serial must fail closed");
        assert!(matches!(
            error.downcast_ref::<NotReconnectableError>(),
            Some(NotReconnectableError::ProtocolViolation(
                OrdinaryMuxProtocolError::DirectionViolation { .. }
            ))
        ));
        assert_eq!(
            pending.map[&serial].stage,
            RpcRetirementStage::AwaitingResponse
        );
        assert_eq!(
            usize::try_from(reader.position()).expect("cursor position fits usize"),
            header_len
        );
        pending.fail_after_decode_error(&error);
        assert!(completion_rx
            .try_recv()
            .expect("terminal cleanup wakes the pending caller")
            .is_err());
        probe.assert_balanced();
    }

    #[test]
    fn inactive_ordered_window_rejection_precedes_serial_ceiling_for_every_serial() {
        let rpc_transport = RpcTransportState::new();
        rpc_transport.mark_current_generation_ready_for_test();
        let generation = rpc_transport
            .active_generation()
            .expect("serial-ceiling test transport starts live");
        let highest_issued = 1;
        let payload = [0x7e; 19];
        for (ident, ordered_window) in [
            (<WindowOrderEventV1 as PduWireIdent>::IDENT, true),
            (<Pong as PduWireIdent>::IDENT, false),
        ] {
            let (wire, header_len) = test_opaque_frame(ident, u64::MAX, false, &payload);
            let mut reader = std::io::Cursor::new(wire);
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                None,
                |header| {
                    validate_ordinary_mux_inbound_header(
                        &rpc_transport,
                        generation,
                        header,
                        highest_issued,
                    )?;
                    Ok(PduBodyDisposition::Materialize)
                },
            ))
            .expect_err("both hostile controls must fail before body admission");

            if ordered_window {
                assert!(matches!(
                    error.downcast_ref::<NotReconnectableError>(),
                    Some(NotReconnectableError::InactiveOrderedWindowPdu {
                        ident: observed_ident,
                        serial: u64::MAX,
                        ..
                    }) if *observed_ident == ident
                ));
            } else {
                assert!(matches!(
                    error.downcast_ref::<CorruptResponse>(),
                    Some(CorruptResponse::SerialAboveCeiling {
                        serial: u64::MAX,
                        max_serial: 1,
                    })
                ));
            }
            assert_eq!(
                usize::try_from(reader.position()).expect("cursor position fits usize"),
                header_len,
                "neither rejection may read its body"
            );
        }
    }

    #[test]
    fn inactive_ordered_window_rejection_wins_before_pending_response_correlation() {
        let ordered_response_ident = <ListPanesOrderedV1Response as PduWireIdent>::IDENT;
        for expected_response_ident in [NonZeroU64::new(ordered_response_ident), None] {
            let (mut pending, probe) = pending_replies_for_test();
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named_expect(
                    completion_tx,
                    "inactive-ordered-window-probe",
                    expected_response_ident,
                )
                .expect("admit response-correlation probe")
                .expect("assign response-correlation probe serial");
            pending
                .set_stage(serial, RpcRetirementStage::AwaitingResponse)
                .expect("mark probe as awaiting its response");

            let payload = [0xcc; 4_096];
            let (wire, header_len) =
                test_opaque_frame(ordered_response_ident, serial.get(), false, &payload);
            let mut reader = std::io::Cursor::new(wire);
            let highest_issued = pending.highest_issued();
            let generation = pending.generation;
            let rpc_transport = Arc::clone(&pending.rpc_transport);
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                None,
                |header| {
                    validate_ordinary_mux_inbound_header(
                        &rpc_transport,
                        generation,
                        header,
                        highest_issued,
                    )?;
                    let correlated_serial = NonZeroU64::new(header.serial())
                        .expect("the response-shaped probe serial is nonzero");
                    let disposition =
                        pending.response_body_disposition(correlated_serial, header)?;
                    Ok(match disposition {
                        PendingResponseBodyDisposition::Materialize(_) => {
                            PduBodyDisposition::Materialize
                        }
                        PendingResponseBodyDisposition::DiscardKnownTombstone => {
                            PduBodyDisposition::Discard
                        }
                    })
                },
            ))
            .expect_err("inactive response-shaped PDU must fail before correlation");

            assert!(matches!(
                error.downcast_ref::<NotReconnectableError>(),
                Some(NotReconnectableError::InactiveOrderedWindowPdu {
                    ident,
                    serial: observed_serial,
                    encoded_payload_len,
                    compressed: false,
                }) if *ident == ordered_response_ident
                    && *observed_serial == serial.get()
                    && *encoded_payload_len == payload.len()
            ));
            assert_eq!(
                pending.map[&serial].stage,
                RpcRetirementStage::AwaitingResponse,
                "inactive-family rejection must precede the ResponseMatch boundary"
            );
            assert_eq!(RpcMetricProbe::counter(&probe.unexpected_response_ident), 0);
            assert_eq!(
                usize::try_from(reader.position()).expect("cursor position fits usize"),
                header_len,
                "inactive response rejection must not consume the body"
            );

            pending.fail_after_decode_error(&error);
            assert!(completion_rx
                .try_recv()
                .expect("terminal teardown must retire the pending caller")
                .is_err());
            probe.assert_balanced();
        }
    }

    #[test]
    fn contextual_inactive_ordered_window_error_remains_no_reconnect_typed() {
        let rejection = NotReconnectableError::InactiveOrderedWindowPdu {
            ident: <WindowOrderEventV1 as PduWireIdent>::IDENT,
            serial: 0,
            encoded_payload_len: 23,
            compressed: true,
        };
        let contextual = anyhow::Error::new(rejection.clone())
            .context("decoding an inbound mux frame")
            .context("ordinary mux client reader terminated");

        assert_eq!(
            contextual.downcast_ref::<NotReconnectableError>(),
            Some(&rejection),
            "the reconnect loop must retain its typed no-reconnect classification through context"
        );
    }

    #[cfg(unix)]
    fn run_inbound_protocol_transport_rejection(
        ident: u64,
        use_request_serial: bool,
        compressed: bool,
        agreed: usize,
    ) -> (anyhow::Error, anyhow::Error) {
        let _watchdog = hang_watchdog(12, "inbound protocol transport gate", 95);
        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create inbound protocol gate socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound inbound protocol gate server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("bound inbound protocol gate server writes");

        let rpc_transport = Arc::new(RpcTransportState::new());
        rpc_transport.mark_current_generation_ready_for_test();
        let generation = rpc_transport
            .active_generation()
            .expect("inbound protocol gate transport starts live");
        rpc_transport.lifecycle.lock().protocol = Some(RpcProtocolAuthority::established_for_test(
            generation, agreed,
        ));
        rpc_transport
            .bind_render_connection_identity(generation, TEST_RENDER_CONNECTION_IDENTITY)
            .expect("test transport should bind its pre-rejection topology identity");
        let connection_generation = Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION));
        let dispatch_authority = ClientDispatchAuthority::new(
            None,
            Weak::new(),
            Arc::new(ClientIncarnation),
            Arc::clone(&connection_generation),
            Arc::clone(&rpc_transport),
        );
        let (sender, receiver) = unbounded();
        let (completion_tx, completion_rx) = bounded(1);
        let prepared = Pdu::Ping(Ping {})
            .prepare_outbound_for_dialect(
                RpcProtocolAuthority::established_for_test(generation, agreed).wire_dialect(),
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("inbound protocol probe should produce an outbound plan");
        let request_name = prepared.pdu().pdu_name();
        let attempt_id = rpc_transport
            .allocate_attempt(request_name)
            .expect("test transport should allocate its probe attempt");
        let lease = rpc_transport
            .outbound_budget
            .try_reserve(Arc::downgrade(&rpc_transport), generation, prepared)
            .expect("inbound protocol probe should fit the outbound budget");
        sender
            .try_send(ReaderMessage::SendPdu {
                binding: RpcBinding {
                    generation,
                    attempt_id,
                    request: request_name,
                    expected_response_ident: NonZeroU64::new(ident),
                },
                lease,
                promise: completion_tx,
            })
            .expect("queue inbound protocol transport probe");

        let unix_domain = UnixDomain {
            name: "inbound-protocol-gate".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(unix_domain),
            Some(Box::new(client_stream)),
        );
        let reader = std::thread::Builder::new()
            .name("inbound-protocol-gate-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn inbound protocol gate reader");

        let request = Pdu::decode(&mut server_stream)
            .expect("server should receive the probe before its hostile response");
        assert!(matches!(request.pdu, Pdu::Ping(_)));
        assert_ne!(request.serial, 0);
        let response_serial = if use_request_serial {
            request.serial
        } else {
            0
        };
        let payload = [0x3c; 8_192];
        let (frame, _) = test_opaque_frame(ident, response_serial, compressed, &payload);
        Write::write_all(&mut server_stream, &frame).expect("write hostile inbound protocol frame");
        Write::flush(&mut server_stream).expect("flush hostile inbound protocol frame");

        let reader_error = reader
            .join()
            .expect("inbound protocol gate reader thread should not panic")
            .expect_err("hostile inbound protocol frame must terminate its reader");

        let caller_error = completion_rx
            .try_recv()
            .expect("inbound protocol gate must wake its pending caller")
            .expect_err("pending caller must lose authority with the transport");
        assert!(matches!(
            caller_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                request: "Ping",
                stage: RpcRetirementStage::AwaitingResponse,
                certainty: RpcDeliveryCertainty::OutcomeUnknown,
                ..
            })
        ));
        assert!(matches!(
            completion_rx.try_recv(),
            Err(async_channel::TryRecvError::Empty) | Err(async_channel::TryRecvError::Closed)
        ));
        assert_eq!(
            rpc_transport.ready_generation.load(AtomicOrdering::Acquire),
            0,
            "protocol rejection must revoke readiness"
        );
        assert_eq!(
            rpc_transport.active_generation(),
            None,
            "protocol rejection must close RPC admission"
        );
        assert_eq!(
            rpc_transport.codec_authority(generation),
            None,
            "protocol rejection must revoke the generation codec authority"
        );
        assert_eq!(
            rpc_transport.render_connection_identity(generation),
            None,
            "protocol rejection must revoke topology/render authority before any mutation"
        );
        assert_eq!(
            connection_generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "the reader alone must neither dial nor publish a successor transport"
        );
        assert!(matches!(
            rpc_transport.lifecycle.lock().phase,
            RpcTransportPhase::Reconnecting { retired, .. } if retired == generation
        ));
        (reader_error, caller_error)
    }

    #[cfg(unix)]
    fn assert_inactive_ordered_window_transport_rejection(
        ident: u64,
        use_request_serial: bool,
        compressed: bool,
    ) {
        let (reader_error, caller_error) = run_inbound_protocol_transport_rejection(
            ident,
            use_request_serial,
            compressed,
            CODEC_VERSION,
        );
        assert_eq!(
            reader_error.downcast_ref::<NotReconnectableError>(),
            Some(&NotReconnectableError::InactiveOrderedWindowPdu {
                ident,
                serial: if use_request_serial { 1 } else { 0 },
                encoded_payload_len: 8_192,
                compressed,
            })
        );
        assert!(matches!(
            caller_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired { reason, .. })
                if reason.contains("inactive ordered-window PDU")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn serial_zero_window_order_event_retires_ordinary_transport_without_body_admission() {
        assert_inactive_ordered_window_transport_rejection(
            <WindowOrderEventV1 as PduWireIdent>::IDENT,
            false,
            true,
        );
    }

    #[cfg(unix)]
    #[test]
    fn correlated_ordered_snapshot_response_retires_ordinary_transport_before_match() {
        assert_inactive_ordered_window_transport_rejection(
            <ListPanesOrderedV1Response as PduWireIdent>::IDENT,
            true,
            false,
        );
    }

    #[cfg(unix)]
    #[test]
    fn above_dialect_reply_retires_exact_transport_without_body_or_successor() {
        let ident = <ListPanesCoherentResponse as PduWireIdent>::IDENT;
        let agreed = LEGACY46_CODEC_VERSION;
        let (reader_error, caller_error) =
            run_inbound_protocol_transport_rejection(ident, true, true, agreed);
        assert!(matches!(
            reader_error.downcast_ref::<NotReconnectableError>(),
            Some(NotReconnectableError::ProtocolViolation(
                OrdinaryMuxProtocolError::DialectViolation {
                    ident: observed_ident,
                    agreed: observed_agreed,
                    ..
                }
            )) if *observed_ident == ident && *observed_agreed == agreed
        ));
        assert!(matches!(
            caller_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired { reason, .. })
                if reason.contains("requires codec dialect")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsolicited_pdu_on_pending_serial_retires_exact_transport_without_successor() {
        let ident = <PaneRemoved as PduWireIdent>::IDENT;
        let (reader_error, caller_error) =
            run_inbound_protocol_transport_rejection(ident, true, false, CODEC_VERSION);
        assert!(matches!(
            reader_error.downcast_ref::<NotReconnectableError>(),
            Some(NotReconnectableError::ProtocolViolation(
                OrdinaryMuxProtocolError::DirectionViolation {
                    ident: observed_ident,
                    role: PduWireRole::CorrelatedReply,
                    ..
                }
            )) if *observed_ident == ident
        ));
        assert!(matches!(
            caller_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired { reason, .. })
                if reason.contains("does not authorize producer")
        ));
    }

    #[test]
    fn abandoned_large_reply_discards_then_resynchronizes_live_and_unilateral_frames() {
        let (mut pending, probe) = pending_replies_for_test();
        let (live_tx, live_rx) = bounded(1);
        let live_serial = pending
            .admit_named_expect(live_tx, "Ping", Some(test_wire_ident::<Pong>()))
            .expect("admit live request")
            .expect("assign live serial");
        let (abandoned_tx, abandoned_rx) = bounded(1);
        let abandoned_serial = pending
            .admit_named_expect(
                abandoned_tx,
                "SearchScrollbackRequest",
                Some(test_wire_ident::<SearchScrollbackResponse>()),
            )
            .expect("admit eventual tombstone")
            .expect("assign abandoned serial");
        drop(abandoned_rx);

        let results = (0_usize..16_384)
            .map(|index| {
                let row = isize::try_from(index).expect("test row fits isize");
                mux::pane::SearchResult {
                    start_y: row,
                    start_x: index,
                    end_y: row,
                    end_x: index.saturating_add(1),
                    match_id: index,
                }
            })
            .collect();
        let mut wire = Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
            .encode_frame_with_mode(abandoned_serial.get(), CompressionMode::Never)
            .expect("encode large uncompressed abandoned response");
        Pdu::PaneRemoved(PaneRemoved { pane_id: 77 })
            .encode(&mut wire, 0)
            .expect("encode unilateral successor");
        Pdu::Pong(Pong {})
            .encode(&mut wire, live_serial.get())
            .expect("encode reordered live successor");
        let mut reader = std::io::Cursor::new(wire);

        asupersync_block_on(async {
            let discarded = Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    let serial = NonZeroU64::new(header.serial())
                        .expect("abandoned response serial is nonzero");
                    assert_eq!(serial, abandoned_serial);
                    assert_eq!(
                        pending.response_body_disposition(serial, header)?,
                        PendingResponseBodyDisposition::DiscardKnownTombstone
                    );
                    Ok(PduBodyDisposition::Discard)
                },
            )
            .await
            .expect("discard abandoned large body");
            let AsyncPduDecode::Discarded {
                serial,
                ident,
                body,
            } = discarded
            else {
                panic!("known tombstone should not materialize its body");
            };
            assert_eq!(serial, abandoned_serial.get());
            assert!(
                pending.map.contains_key(&abandoned_serial),
                "tombstone must remain correlated until its entire body drains"
            );
            assert!(body.encoded_bytes() > DiscardedPduBody::scratch_capacity());
            assert_eq!(body.max_chunk_bytes(), DiscardedPduBody::scratch_capacity());
            assert!(body.chunk_reads() > 1);
            pending
                .complete_discarded_abandoned(abandoned_serial, ident)
                .expect("retire exact drained tombstone");

            let unilateral = Pdu::decode_async(&mut reader, Some(pending.highest_issued()))
                .await
                .expect("unilateral frame after discard remains aligned");
            assert_eq!(unilateral.serial, 0);
            assert_eq!(
                unilateral.pdu,
                Pdu::PaneRemoved(PaneRemoved { pane_id: 77 })
            );

            let mut live_effect = None;
            let live = Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    let serial =
                        NonZeroU64::new(header.serial()).expect("live response serial is nonzero");
                    match pending.response_body_disposition(serial, header)? {
                        PendingResponseBodyDisposition::Materialize(effect) => {
                            live_effect = Some(effect);
                            Ok(PduBodyDisposition::Materialize)
                        }
                        PendingResponseBodyDisposition::DiscardKnownTombstone => {
                            panic!("live waiter must never authorize discard")
                        }
                    }
                },
            )
            .await
            .expect("decode live response after abandoned body");
            let AsyncPduDecode::Decoded(live) = live else {
                panic!("live response must be materialized");
            };
            assert_eq!(live.serial, live_serial.get());
            assert_eq!(live.pdu, Pdu::Pong(Pong {}));
            assert_eq!(live_effect, Some(PendingRpcEffect::Ordinary));
            pending
                .complete(live_serial, PendingRpcReply::pdu(live.pdu))
                .expect("deliver live response");
        });

        assert!(matches!(
            live_rx
                .try_recv()
                .expect("live response must reach caller")
                .expect("live response must be successful"),
            PendingRpcReply::Pdu(pdu) if matches!(pdu.as_ref(), Pdu::Pong(Pong {}))
        ));
        assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        probe.assert_balanced();
    }

    #[test]
    fn abandoned_typed_error_response_is_always_materialized() {
        let (mut pending, probe) = pending_replies_for_test();
        let (completion_tx, completion_rx) = bounded(1);
        let serial = pending
            .admit_named_expect(completion_tx, "Ping", Some(test_wire_ident::<Pong>()))
            .expect("admit typed request")
            .expect("assign typed serial");
        drop(completion_rx);

        let response = ErrorResponse::backend_failure(Ping::IDENT);
        let wire = Pdu::ErrorResponse(response.clone())
            .encode_frame_with_mode(serial.get(), CompressionMode::Never)
            .expect("encode uncompressed error response");
        let mut reader = std::io::Cursor::new(wire);

        let decoded = asupersync_block_on(Pdu::decode_async_with_selector(
            &mut reader,
            Some(pending.highest_issued()),
            |header| {
                assert_eq!(header.serial(), serial.get());
                assert_eq!(header.ident(), <ErrorResponse as PduWireIdent>::IDENT);
                assert_eq!(
                    pending.response_body_disposition(serial, header)?,
                    PendingResponseBodyDisposition::Materialize(PendingRpcEffect::Ordinary)
                );
                Ok(PduBodyDisposition::Materialize)
            },
        ))
        .expect("error response must retain full decoding");
        let AsyncPduDecode::Decoded(decoded) = decoded else {
            panic!("ErrorResponse must never use raw body discard");
        };
        assert_eq!(decoded.pdu, Pdu::ErrorResponse(response));
        assert_eq!(
            pending
                .complete(serial, PendingRpcReply::pdu(decoded.pdu))
                .expect("retire abandoned decoded ErrorResponse")
                .disposition,
            ReplyDisposition::Abandoned
        );
        assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
        probe.assert_balanced();
    }

    #[test]
    fn discard_classifier_preserves_compressed_generic_and_topology_frames() {
        {
            let (mut pending, probe) = pending_replies_for_test();
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named_expect(completion_tx, "Ping", Some(test_wire_ident::<Pong>()))
                .expect("admit typed compressed request")
                .expect("assign typed compressed serial");
            drop(completion_rx);
            let wire = Pdu::Pong(Pong {})
                .encode_frame_with_mode(serial.get(), CompressionMode::Always)
                .expect("encode compressed typed response");
            let mut reader = std::io::Cursor::new(wire);

            let decoded = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    assert!(header.is_compressed());
                    assert_eq!(
                        pending.response_body_disposition(serial, header)?,
                        PendingResponseBodyDisposition::Materialize(PendingRpcEffect::Ordinary)
                    );
                    Ok(PduBodyDisposition::Materialize)
                },
            ))
            .expect("compressed tombstone must retain decompression and schema validation");
            let AsyncPduDecode::Decoded(decoded) = decoded else {
                panic!("compressed tombstone must never use raw body discard");
            };
            pending
                .complete(serial, PendingRpcReply::pdu(decoded.pdu))
                .expect("retire decoded compressed tombstone");
            assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
            probe.assert_balanced();
        }

        {
            let (mut pending, probe) = pending_replies_for_test();
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named(completion_tx, "generic-send-pdu")
                .expect("admit generic request")
                .expect("assign generic serial");
            drop(completion_rx);
            let wire = Pdu::Pong(Pong {})
                .encode_frame_with_mode(serial.get(), CompressionMode::Never)
                .expect("encode generic response");
            let mut reader = std::io::Cursor::new(wire);

            let decoded = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    assert_eq!(
                        pending.response_body_disposition(serial, header)?,
                        PendingResponseBodyDisposition::Materialize(PendingRpcEffect::Ordinary)
                    );
                    Ok(PduBodyDisposition::Materialize)
                },
            ))
            .expect("generic request response must retain full decoding");
            let AsyncPduDecode::Decoded(decoded) = decoded else {
                panic!("generic request has no exact typed discard authority");
            };
            pending
                .complete(serial, PendingRpcReply::pdu(decoded.pdu))
                .expect("retire decoded generic response");
            assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
            probe.assert_balanced();
        }

        {
            let (mut pending, probe) = pending_replies_for_test();
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named_expect(completion_tx, "Ping", Some(test_wire_ident::<Pong>()))
                .expect("admit topology-fence request")
                .expect("assign topology-fence serial");
            pending
                .map
                .get_mut(&serial)
                .expect("admitted topology-fence request remains pending")
                .effect = PendingRpcEffect::CoherentTopologyFence;
            drop(completion_rx);
            let wire = Pdu::Pong(Pong {})
                .encode_frame_with_mode(serial.get(), CompressionMode::Never)
                .expect("encode topology-fence response");
            let mut reader = std::io::Cursor::new(wire);

            let decoded = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    assert_eq!(
                        pending.response_body_disposition(serial, header)?,
                        PendingResponseBodyDisposition::Materialize(
                            PendingRpcEffect::CoherentTopologyFence
                        )
                    );
                    Ok(PduBodyDisposition::Materialize)
                },
            ))
            .expect("topology-fence response must retain full decoding");
            let AsyncPduDecode::Decoded(decoded) = decoded else {
                panic!("topology-fence response must never use raw body discard");
            };
            pending
                .complete(serial, PendingRpcReply::pdu(decoded.pdu))
                .expect("retire decoded topology-fence response");
            assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 1);
            probe.assert_balanced();
        }
    }

    #[test]
    fn typed_wrong_response_ident_is_fatal_before_body_for_live_and_closed_waiters() {
        let known_wrong_ident = <SearchScrollbackResponse as PduWireIdent>::IDENT;
        let unknown_ident = 127_u64;
        assert_eq!(Pdu::pdu_name_for_ident(unknown_ident), None);

        for &(caller_is_closed, observed_ident) in &[
            (false, known_wrong_ident),
            (false, unknown_ident),
            (true, known_wrong_ident),
            (true, unknown_ident),
        ] {
            let (mut pending, probe) = pending_replies_for_test();
            let (completion_tx, completion_rx) = bounded(1);
            let serial = pending
                .admit_named_expect(completion_tx, "Ping", Some(test_wire_ident::<Pong>()))
                .expect("admit typed request")
                .expect("assign typed serial");
            let completion_rx = if caller_is_closed {
                drop(completion_rx);
                None
            } else {
                Some(completion_rx)
            };

            let mut wire = Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                results: vec![mux::pane::SearchResult {
                    start_y: 1,
                    start_x: 2,
                    end_y: 1,
                    end_x: 3,
                    match_id: 4,
                }],
            })
            .encode_frame_with_mode(serial.get(), CompressionMode::Never)
            .expect("encode wrong typed response");
            if observed_ident == unknown_ident {
                let length_field_len = wire
                    .iter()
                    .position(|byte| byte & 0x80 == 0)
                    .map(|index| index.saturating_add(1))
                    .expect("encoded frame length has a terminating LEB128 byte");
                let serial_field_len = wire[length_field_len..]
                    .iter()
                    .position(|byte| byte & 0x80 == 0)
                    .map(|index| index.saturating_add(1))
                    .expect("encoded serial has a terminating LEB128 byte");
                let ident_offset = length_field_len.saturating_add(serial_field_len);
                let ident_byte = wire
                    .get_mut(ident_offset)
                    .expect("encoded frame contains an identifier byte");
                assert_eq!(
                    u64::from(*ident_byte),
                    known_wrong_ident,
                    "test mutation assumes both identifiers use one-byte LEB128"
                );
                *ident_byte = u8::try_from(unknown_ident).expect("unknown test ident fits u8");
            }
            let wire_len = wire.len();
            let mut reader = std::io::Cursor::new(wire);
            let error = asupersync_block_on(Pdu::decode_async_with_selector(
                &mut reader,
                Some(pending.highest_issued()),
                |header| {
                    let serial =
                        NonZeroU64::new(header.serial()).expect("typed response serial is nonzero");
                    let disposition = pending.response_body_disposition(serial, header)?;
                    Ok(match disposition {
                        PendingResponseBodyDisposition::Materialize(_) => {
                            PduBodyDisposition::Materialize
                        }
                        PendingResponseBodyDisposition::DiscardKnownTombstone => {
                            PduBodyDisposition::Discard
                        }
                    })
                },
            ))
            .expect_err("wrong response ident must fail before body consumption");
            assert!(matches!(
                error.downcast_ref::<PendingRpcError>(),
                Some(PendingRpcError::UnexpectedResponseIdent {
                    serial: observed,
                    request: "Ping",
                    expected_response_ident,
                    observed_ident: rejected_ident,
                }) if *observed == serial
                    && *rejected_ident == observed_ident
                    && *expected_response_ident == <Pong as PduWireIdent>::IDENT
            ));
            assert!(
                usize::try_from(reader.position()).expect("cursor position fits usize") < wire_len,
                "wrong-ident rejection must leave the payload unread for terminal teardown"
            );
            assert!(pending.map.contains_key(&serial));
            assert_eq!(
                pending.map[&serial].stage,
                RpcRetirementStage::ResponseMatch,
                "header correlation must advance retirement diagnostics before failure"
            );
            assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 0);
            assert_eq!(RpcMetricProbe::counter(&probe.unexpected_response_ident), 1);

            pending.fail_after_decode_error(&error);
            if let Some(completion_rx) = completion_rx {
                assert!(completion_rx
                    .try_recv()
                    .expect("live waiter must be woken by terminal teardown")
                    .is_err());
            }
            assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 0);
            probe.assert_balanced();
        }
    }

    #[test]
    fn matched_header_records_response_stage_before_retirement_validation() {
        let (mut pending, probe) = pending_replies_for_test();
        let (completion_tx, completion_rx) = bounded(1);
        let serial = pending
            .admit_named_expect(
                completion_tx,
                "SearchScrollbackRequest",
                Some(test_wire_ident::<SearchScrollbackResponse>()),
            )
            .expect("admit typed request")
            .expect("assign typed serial");
        let wire = Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
            results: vec![mux::pane::SearchResult {
                start_y: 1,
                start_x: 2,
                end_y: 1,
                end_x: 3,
                match_id: 4,
            }],
        })
        .encode_frame_with_mode(serial.get(), CompressionMode::Never)
        .expect("encode response whose header will race retirement");
        let wire_len = wire.len();
        let mut reader = std::io::Cursor::new(wire);

        pending.rpc_transport.mark_incarnation_terminal(
            RpcTransportError::AttemptIdentityExhausted {
                request: "retirement-race",
            },
        );
        let error = asupersync_block_on(Pdu::decode_async_with_selector(
            &mut reader,
            Some(pending.highest_issued()),
            |header| {
                assert_eq!(header.serial(), serial.get());
                let disposition = pending.response_body_disposition(serial, header)?;
                Ok(match disposition {
                    PendingResponseBodyDisposition::Materialize(_) => {
                        PduBodyDisposition::Materialize
                    }
                    PendingResponseBodyDisposition::DiscardKnownTombstone => {
                        PduBodyDisposition::Discard
                    }
                })
            },
        ))
        .expect_err("retirement at exact header correlation must fail closed");

        assert!(matches!(
            error.downcast_ref::<PendingRpcError>(),
            Some(PendingRpcError::IncarnationTerminal(
                RpcTransportError::Retired {
                    request: "SearchScrollbackRequest",
                    stage: RpcRetirementStage::ResponseMatch,
                    certainty: RpcDeliveryCertainty::OutcomeUnknown,
                    reason,
                    ..
                }
            )) if reason == "transport retired before response body admission"
        ));
        assert!(
            usize::try_from(reader.position()).expect("cursor position fits usize") < wire_len,
            "retirement validation must leave the response body unread"
        );
        assert_eq!(
            pending.map[&serial].stage,
            RpcRetirementStage::ResponseMatch,
            "header correlation must be visible even when transport validation fails"
        );

        pending.fail_after_decode_error(&error);
        assert!(completion_rx
            .try_recv()
            .expect("retirement must wake the live response waiter")
            .is_err());
        probe.assert_balanced();
    }

    #[test]
    fn fatal_reply_dispositions_fail_every_other_live_waiter() {
        let (mut duplicate_pending, duplicate_probe) = pending_replies_for_test();
        let (completed_tx, completed_rx) = bounded(1);
        let completed_serial = duplicate_pending
            .admit_named(completed_tx, "Ping")
            .expect("admit first request")
            .expect("assign first serial");
        let (duplicate_witness_tx, duplicate_witness_rx) = bounded(1);
        duplicate_pending
            .admit_named(duplicate_witness_tx, "GetCodecVersion")
            .expect("admit duplicate-failure witness")
            .expect("assign witness serial");
        duplicate_pending
            .complete(completed_serial, PendingRpcReply::pdu(Pdu::Pong(Pong {})))
            .expect("first reply should enqueue");
        assert!(completed_rx.try_recv().expect("first completion").is_ok());

        let duplicate_error = duplicate_pending
            .complete_or_fail_transport(completed_serial, Pdu::Pong(Pong {}))
            .expect_err("duplicate reply must retire the transport");
        assert!(matches!(
            duplicate_error,
            PendingRpcError::UnmatchedSerial { serial, .. } if serial == completed_serial
        ));
        assert!(duplicate_witness_rx
            .try_recv()
            .expect("duplicate reply must wake every other live waiter")
            .is_err());
        duplicate_probe.assert_balanced();

        let (mut full_pending, full_probe) = pending_replies_for_test();
        let (full_tx, full_rx) = bounded(1);
        let filler = full_tx.clone();
        let full_serial = full_pending
            .admit_named(full_tx, "Ping")
            .expect("admit full-channel request")
            .expect("assign full-channel serial");
        let (full_witness_tx, full_witness_rx) = bounded(1);
        full_pending
            .admit_named(full_witness_tx, "GetCodecVersion")
            .expect("admit full-channel witness")
            .expect("assign full-channel witness serial");
        filler
            .try_send(Ok(PendingRpcReply::pdu(Pdu::Pong(Pong {}))))
            .expect("prefill completion channel");

        let full_error = full_pending
            .complete_or_fail_transport(full_serial, Pdu::Pong(Pong {}))
            .expect_err("full reply channel must retire the transport");
        assert!(matches!(
            full_error,
            PendingRpcError::ReplyChannelFull { serial, .. } if serial == full_serial
        ));
        assert!(full_rx
            .try_recv()
            .expect("prefilled reply must remain intact")
            .is_ok());
        assert!(full_witness_rx
            .try_recv()
            .expect("full reply channel must wake every other live waiter")
            .is_err());
        full_probe.assert_balanced();
    }

    #[test]
    fn codec_future_serial_rejection_reaches_client_protocol_metric() {
        let (mut pending, probe) = pending_replies_for_test();
        let (live_tx, live_rx) = bounded(1);
        let admitted_serial = pending
            .admit_named(live_tx, "Ping")
            .expect("admit live request")
            .expect("assign live serial");
        let future_serial = admitted_serial
            .get()
            .checked_add(1)
            .expect("test serial has headroom");

        let mut encoded = Vec::new();
        Pdu::Pong(Pong {})
            .encode(&mut encoded, future_serial)
            .expect("encode future response");
        let mut reader = std::io::Cursor::new(encoded);
        let error = asupersync_block_on(Pdu::decode_async(
            &mut reader,
            Some(pending.highest_issued()),
        ))
        .expect_err("codec must reject a response above the transport high-water mark");
        assert_eq!(
            error.downcast_ref::<CorruptResponse>(),
            Some(&CorruptResponse::SerialAboveCeiling {
                serial: future_serial,
                max_serial: admitted_serial.get(),
            })
        );

        pending.fail_after_decode_error(&error);
        assert_eq!(RpcMetricProbe::counter(&probe.future_serial), 1);
        assert_eq!(
            probe.pending(),
            0.0,
            "header rejection must retire the transport and clear every waiter"
        );
        assert!(live_rx
            .try_recv()
            .expect("transport teardown must wake the admitted waiter")
            .is_err());
        probe.assert_balanced();
    }

    #[test]
    fn pending_rpc_transport_teardown_wakes_live_and_clears_abandoned_waiters() {
        let (mut pending, probe) = pending_replies_for_test();
        let (live_tx, live_rx) = bounded(1);
        pending
            .admit_named(live_tx, "Ping")
            .expect("admit live waiter")
            .expect("assign live waiter serial");
        let (abandoned_tx, abandoned_rx) = bounded(1);
        pending
            .admit_named(abandoned_tx, "SearchScrollbackRequest")
            .expect("admit eventual abandonment")
            .expect("assign abandoned waiter serial");
        drop(abandoned_rx);

        for pending_rpc in pending.map.values_mut() {
            pending_rpc.stage = RpcRetirementStage::BeforeFlush;
        }

        let transport_error = anyhow!("test transport terminated during flush");
        pending.fail_after_transport_error(&transport_error);
        pending.fail_after_transport_error(&transport_error);
        drop(pending);

        let live_error = live_rx
            .try_recv()
            .expect("transport teardown should wake live waiter")
            .expect_err("transport teardown should report an error");
        assert!(
            live_error
                .to_string()
                .contains("test transport terminated during flush"),
            "unexpected transport error: {:#}",
            live_error
        );
        assert!(
            !live_error.to_string().contains("Client was destroyed"),
            "transport failure must preserve its real cause"
        );
        assert!(matches!(
            live_error.downcast_ref::<RpcTransportError>(),
            Some(RpcTransportError::Retired {
                stage: RpcRetirementStage::BeforeFlush,
                certainty: RpcDeliveryCertainty::OutcomeUnknown,
                ..
            })
        ));
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 1);
        assert_eq!(
            RpcMetricProbe::counter(&probe.transport_cleared_abandoned),
            1
        );
        assert!(matches!(
            live_rx.try_recv(),
            Err(async_channel::TryRecvError::Closed)
        ));
        assert_eq!(probe.pending(), 0.0);
        probe.assert_balanced();
    }

    #[test]
    fn remote_cli_default_uses_only_the_implemented_wezterm_cli() {
        let command = Reconnectable::build_remote_wezterm_cli_command(&None, "--version")
            .expect("build remote WezTerm CLI command");

        assert_eq!(command, "exec wezterm cli --version");
        assert!(!command.contains("/current/ft"));
        assert!(!command.contains("exec ft cli"));
    }

    /// ft-7f2om: the IncompatibleVersionError Display impl must surface
    /// both the local and remote codec versions plus a pointer to the
    /// compatibility-window diagnosis and atomic-redeploy operator runbook so
    /// on-call sees the runbook path the moment a handshake fails. The
    /// pre-ft-7f2om message
    /// said "install the same version of wezterm" — outdated framing
    /// (we retired the wezterm-as-identity framing in ft-zoxxq.3) and
    /// gave operators no pointer to the new ft-kuxho docs trio.
    #[test]
    fn incompatible_version_error_includes_versions_and_runbook_link() {
        let err = IncompatibleVersionError {
            version: "ft 0.99.99".to_string(),
            codec_vers: 47,
            remote_min_supported: 46,
        };
        let rendered = err.to_string();

        // Local-side codec version (CODEC_VERSION constant) must appear
        // verbatim. We don't hard-code the literal value because it
        // moves over time; instead we read the same constant the impl
        // reads and assert the formatted string contains it.
        let local_codec = CODEC_VERSION.to_string();
        assert!(
            rendered.contains(&local_codec),
            "rendered error missing local CODEC_VERSION ({}): {}",
            local_codec,
            rendered
        );

        // Remote-side codec version (the field value) must appear too.
        assert!(
            rendered.contains("47"),
            "rendered error missing remote codec_vers (47): {}",
            rendered
        );
        assert!(
            rendered.contains("46..=47"),
            "rendered error missing the complete remote codec window: {}",
            rendered
        );

        // Remote frankenterm version string must appear so operators
        // can correlate against deploy bundles.
        assert!(
            rendered.contains("ft 0.99.99"),
            "rendered error missing remote version string: {}",
            rendered
        );

        // Runbook link must appear so on-call has a one-click path to
        // the operator procedure. The exact docs path is part of the
        // user-facing contract — change it deliberately or this test
        // fails.
        assert!(
            rendered.contains("docs/codec-atomic-redeploy.md"),
            "rendered error missing docs/codec-atomic-redeploy.md link: {}",
            rendered
        );
        assert!(
            rendered.contains("drain its live PTYs")
                && rendered.contains("roll back the desktop client")
                && rendered.contains("no automatic mux restart was attempted"),
            "rendered error missing safe upgrade/rollback remediation: {}",
            rendered
        );
        assert!(
            rendered.contains("compatibility windows do not overlap"),
            "rendered error must explain the negotiated-window failure: {}",
            rendered
        );

        // The retired "install the same version of wezterm" framing
        // (per ft-zoxxq.3) must NOT come back. Guard against accidental
        // reverts.
        assert!(
            !rendered.contains("install the same version of wezterm"),
            "retired ft-zoxxq.3 framing reintroduced in IncompatibleVersionError: {}",
            rendered
        );
    }

    #[test]
    fn unsupported_codec_rejects_before_identity_topology_or_readiness() {
        let (client, receiver) = client_with_bootstrap_rpc_queue();
        let ui = ConnectionUI::new_headless();
        const {
            assert!(
                codec::CODEC_VERSION_MIN_SUPPORTED >= TOPOLOGY_FENCE_MIN_CODEC_VERSION,
                "the supported codec window must not admit a pre-topology-fence peer"
            );
        }
        let rejected_codec_version = codec::CODEC_VERSION_MIN_SUPPORTED
            .checked_sub(1)
            .expect("the supported codec floor is nonzero");
        let rejected_generation = client
            .rpc_transport
            .active_generation()
            .expect("version-gate test transport starts live");
        let rejected_reader_abort = client
            .rpc_transport
            .reader_abort_for(rejected_generation)
            .expect("version-gate test has exact reader abort authority");

        let (result, transcript) = asupersync_block_on(async {
            let verify = client.verify_version_compat(&ui);
            let peer = async {
                let first = receiver
                    .recv()
                    .await
                    .expect("version gate must issue one bootstrap RPC");
                let ReaderMessage::SendPdu {
                    binding,
                    lease,
                    promise,
                } = first
                else {
                    panic!("version gate must begin with GetCodecVersion");
                };
                let prepared = lease
                    .claim_for_reader()
                    .expect("version-gate reader claim should remain coherent")
                    .expect("version-gate request must retain its exact PDU");
                assert_eq!(binding.request, "GetCodecVersion");
                assert!(matches!(prepared.pdu(), Pdu::GetCodecVersion(_)));
                promise
                    .send(Ok(PendingRpcReply::pdu(Pdu::GetCodecVersionResponse(
                        GetCodecVersionResponse {
                            codec_vers: rejected_codec_version,
                            version_string: "below-supported-window-peer".to_string(),
                            executable_path: PathBuf::from("/test/old-ft"),
                            config_file_path: None,
                            min_supported: rejected_codec_version,
                        },
                    ))))
                    .await
                    .expect("version response consumer must remain live");

                assert!(
                    matches!(receiver.try_recv(), Err(async_channel::TryRecvError::Empty)),
                    "version rejection must not enqueue SetClientId, topology, or readiness work"
                );
                ["GetCodecVersion"]
            };
            futures::future::join(verify, peer).await
        });

        let error = result.expect_err("a peer below the supported codec floor must be rejected");
        let rejection = error
            .downcast_ref::<IncompatibleVersionError>()
            .expect("codec-window rejection must retain its typed error");
        assert_eq!(rejection.codec_vers, rejected_codec_version);
        assert_eq!(rejection.remote_min_supported, rejected_codec_version);
        assert_eq!(transcript, ["GetCodecVersion"]);
        assert_eq!(
            rejected_reader_abort.cause(),
            Some("standalone mux RPC bootstrap failed, timed out, or was cancelled")
        );
        assert_eq!(
            client
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            0
        );
        assert_eq!(
            client.rpc_transport.codec_authority(rejected_generation),
            None,
            "an unsupported peer must be rejected before codec authority is retained"
        );
        assert_eq!(
            client
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0,
            "a rejected mixed-version attachment must never publish readiness"
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_version_compat_reaches_set_client_id_over_real_stream_ft_kuxho_4() {
        reset_test_logger();

        let socket_path = unique_handshake_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind local UDS handshake server");
        let (set_client_id_tx, set_client_id_rx) = mpsc::channel::<SetClientId>();
        let (server_release_tx, server_release_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-kuxho-handshake-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let (mut stream, _addr) = listener.accept().context("accept mux client")?;

                loop {
                    let decoded = Pdu::decode(&mut stream).context("server decode client PDU")?;

                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION + 1,
                                version_string: "ft-kuxho.4-real-stream-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            })
                        }
                        Pdu::SetClientId(set_client_id) => {
                            set_client_id_tx
                                .send(set_client_id)
                                .expect("test receiver should remain alive");
                            Pdu::UnitResponse(UnitResponse {})
                        }
                        other => panic!("unexpected client handshake PDU: {}", other.pdu_name()),
                    };

                    response
                        .encode(&mut stream, decoded.serial)
                        .context("server encode response PDU")?;
                    stream.flush().context("server flush response PDU")?;

                    if matches!(response, Pdu::UnitResponse(_)) {
                        // Keep the transport alive until the test has consumed
                        // the response. Closing here races the handshake future
                        // against the reader's terminal EOF result and makes
                        // the harness nondeterministic even when the response
                        // was decoded and delivered correctly.
                        server_release_rx
                            .recv_timeout(Duration::from_secs(5))
                            .context("wait for completed client handshake")?;
                        break;
                    }
                }

                Ok(())
            })
            .expect("spawn local UDS handshake server");

        let mut ui = ConnectionUI::new_headless();
        let unix_domain = UnixDomain {
            name: "ft-kuxho.4".to_string(),
            socket_path: Some(socket_path),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_domain.clone()), None);
        reconnectable
            .connect(true, &mut ui, true)
            .expect("connect to local UDS handshake server");
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, mut receiver) = unbounded();
        let client = Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport: Arc::new(RpcTransportState::new()),
            domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        };
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        let info = asupersync_block_on(async {
            let handshake = client.verify_version_compat(&ui);
            let worker =
                client_thread_async(&mut reconnectable, &mut receiver, &dispatch_authority);
            pin_mut!(handshake);
            pin_mut!(worker);
            match select(handshake, worker).await {
                Either::Left((result, _worker)) => result,
                Either::Right((result, _handshake)) => {
                    panic!(
                        "client thread ended before handshake completed: {:?}",
                        result
                    )
                }
            }
        })
        .expect("v+1 server with min_supported=v must complete client handshake");

        assert_eq!(info.codec_vers, CODEC_VERSION + 1);
        assert_eq!(info.min_supported, CODEC_VERSION);

        let set_client_id = set_client_id_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server should see client id registration");
        assert_eq!(set_client_id.client_id, client.client_id);
        assert!(!set_client_id.is_proxy);

        let logs = captured_logs().join("\n");
        let expected_warning = format!(
            "Codec compat window: server={}, client={}, agreed={}",
            CODEC_VERSION + 1,
            CODEC_VERSION,
            CODEC_VERSION,
        );
        assert!(
            logs.contains(&expected_warning),
            "expected exact in-window negotiation warning {:?}, got logs: {}",
            expected_warning,
            logs,
        );

        server_release_tx
            .send(())
            .expect("server must remain alive through handshake assertions");
        drop(client);
        server
            .join()
            .expect("server thread should join")
            .expect("server handshake loop should succeed");
    }

    /// Regression for the mux-client connect hang: the reader must receive
    /// socket-readiness wakeups even when the server's reply arrives AFTER the
    /// reader parks (i.e. any real, latency-bearing connection). Before the fix
    /// the reader ran as a directly-polled `block_on` future, which asupersync
    /// never delivers I/O wakeups to, so a delayed handshake reply was never
    /// consumed and `verify_version_compat` timed out. `client_thread` now runs
    /// the reader as a scheduler-managed task via `block_on_io`. This test
    /// drives the REAL `client_thread` against a server that delays its reply,
    /// so it hangs (watchdog-killed) on regression and completes on success.
    /// Regression guard for the connect hang (#1). The mux reader must receive
    /// socket-readiness wakeups even when the server reply arrives AFTER it
    /// parks (any real, latency-bearing connection). `client_thread` runs the
    /// reader via `block_on_io` (a scheduler-managed, reactor-driven task);
    /// before that it was a directly-polled `block_on` future, which asupersync
    /// never delivers fd readiness to, so a delayed reply hung forever.
    ///
    /// NB: this is deliberately a *real fd* test (UDS + a wall-clock watchdog),
    /// NOT an asupersync `LabRuntime` test. LabRuntime models logical concurrency
    /// / scheduling (DPOR) for simulated tasks; it does not model the OS fd
    /// reactor whose readiness delivery is the actual bug here, so a LabRuntime
    /// test would prove nothing about this regression. The watchdog converts a
    /// hang into a fast, explicit failure.
    #[cfg(unix)]
    #[test]
    fn reader_receives_delayed_handshake_reply_ft_connect_fix() {
        // Watchdog: if the reader never wakes, fail fast instead of waiting out
        // the 60s in-flight handshake timeout.
        let _wd = hang_watchdog(12, "delayed-handshake reader (readiness regression)", 98);

        let socket_path = unique_handshake_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind local UDS handshake server");
        let (server_release_tx, server_release_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-connect-fix-delayed-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let (mut stream, _addr) = listener.accept().context("accept mux client")?;
                let mut first = true;
                loop {
                    let decoded = Pdu::decode(&mut stream).context("server decode client PDU")?;
                    if first {
                        // Reply only AFTER the reader has parked waiting for
                        // readability — the condition the old code hung on.
                        std::thread::sleep(Duration::from_millis(400));
                        first = false;
                    }
                    let response = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => {
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "ft-connect-fix-delayed-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            })
                        }
                        Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                        other => panic!("unexpected client handshake PDU: {}", other.pdu_name()),
                    };
                    response
                        .encode(&mut stream, decoded.serial)
                        .context("server encode response PDU")?;
                    stream.flush().context("server flush response PDU")?;
                    if matches!(response, Pdu::UnitResponse(_)) {
                        server_release_rx
                            .recv_timeout(Duration::from_secs(5))
                            .context("hold delayed server through readiness publication")?;
                        break;
                    }
                }
                Ok(())
            })
            .expect("spawn delayed UDS handshake server");

        let mut ui = ConnectionUI::new_headless();
        let unix_domain = UnixDomain {
            name: "ft-connect-fix".to_string(),
            socket_path: Some(socket_path),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut reconnectable = Reconnectable::new(ClientDomainConfig::Unix(unix_domain), None);
        reconnectable
            .connect(true, &mut ui, true)
            .expect("connect to local UDS handshake server");
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, receiver) = unbounded();
        let client = Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport: Arc::new(RpcTransportState::new()),
            domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        };
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        // Run the REAL reader exactly as production does (client_thread ->
        // block_on_io -> scheduler-managed, reactor-driven task).
        let reader = std::thread::Builder::new()
            .name("ft-connect-fix-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn reader thread");

        let info = asupersync_block_on(client.verify_version_compat(&ui))
            .expect("delayed handshake reply must be consumed by the reader");
        assert_eq!(info.codec_vers, CODEC_VERSION);

        server_release_tx
            .send(())
            .expect("release delayed server after readiness acknowledgement");
        drop(client);
        assert_expected_reader_shutdown(reader.join().expect("reader thread panicked"));
        server
            .join()
            .expect("server thread should join")
            .expect("server handshake loop should succeed");
    }

    /// A caller dropping an RPC is abandonment, not transport destruction.
    ///
    /// This socket-pair regression covers both cancellation sides of the
    /// reader's admission boundary through the production reconnect loop and
    /// deliberately reorders multiple live and abandoned replies. The
    /// preclosed request must consume neither a frame nor serial. Requests
    /// dropped after the server confirms receipt remain as tombstones until
    /// their replies are drained. Distinct live response types prove exact
    /// serial correlation, and a final Ping proves that every operation used
    /// the same socket, reader, and connection generation.
    #[cfg(unix)]
    #[test]
    fn abandoned_rpc_replies_drain_without_retiring_transport_generation() {
        fn recv_rpc_with_timeout(
            receiver: &Receiver<anyhow::Result<PendingRpcReply>>,
            label: &str,
        ) -> Pdu {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match receiver.try_recv() {
                    Ok(result) => {
                        return match result.unwrap_or_else(|err| panic!("{}: {:#}", label, err)) {
                            PendingRpcReply::Pdu(pdu) => *pdu,
                            other => panic!(
                                "{}: received unexpected typed reply {}",
                                label,
                                other.response_name()
                            ),
                        };
                    }
                    Err(async_channel::TryRecvError::Closed) => {
                        panic!("{}: completion channel closed without a response", label)
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{}: timed out waiting for RPC completion",
                            label
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }

        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create in-memory mux socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("bound server writes");
        let (requests_admitted_tx, requests_admitted_rx) = mpsc::channel::<()>();
        let (release_replies_tx, release_replies_rx) = mpsc::channel::<()>();
        let (release_server_tx, release_server_rx) = mpsc::channel::<()>();
        let (server_done_tx, server_done_rx) = mpsc::channel::<()>();
        let server = std::thread::Builder::new()
            .name("ft-rpc-abandonment-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                let result = (|| -> anyhow::Result<()> {
                    let first =
                        Pdu::decode(&mut server_stream).context("decode first admitted RPC")?;
                    let second =
                        Pdu::decode(&mut server_stream).context("decode second admitted RPC")?;
                    let third =
                        Pdu::decode(&mut server_stream).context("decode third admitted RPC")?;
                    let fourth =
                        Pdu::decode(&mut server_stream).context("decode fourth admitted RPC")?;

                    anyhow::ensure!(first.serial == 1, "preclosed request consumed a serial");
                    anyhow::ensure!(second.serial == 2, "unexpected second serial");
                    anyhow::ensure!(third.serial == 3, "unexpected third serial");
                    anyhow::ensure!(fourth.serial == 4, "unexpected fourth serial");
                    anyhow::ensure!(
                        matches!(first.pdu, Pdu::SearchScrollbackRequest(_)),
                        "unexpected first request"
                    );
                    anyhow::ensure!(
                        matches!(second.pdu, Pdu::GetTlsCreds(_)),
                        "unexpected second request"
                    );
                    anyhow::ensure!(
                        matches!(third.pdu, Pdu::SearchScrollbackRequest(_)),
                        "unexpected third request"
                    );
                    anyhow::ensure!(
                        matches!(fourth.pdu, Pdu::Ping(_)),
                        "unexpected fourth request"
                    );

                    requests_admitted_tx
                        .send(())
                        .context("notify caller of wire admission")?;
                    release_replies_rx
                        .recv_timeout(Duration::from_secs(10))
                        .context("wait for caller abandonment")?;

                    Pdu::Pong(Pong {})
                        .encode(&mut server_stream, fourth.serial)
                        .context("encode fourth response")?;
                    Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    })
                    .encode(&mut server_stream, third.serial)
                    .context("encode third response")?;
                    Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                        ca_cert_pem: "ft-rpc-correlation-ca".to_string(),
                        client_cert_pem: "ft-rpc-correlation-client".to_string(),
                    })
                    .encode(&mut server_stream, second.serial)
                    .context("encode second response")?;
                    Pdu::SearchScrollbackResponse(SearchScrollbackResponse {
                        results: Vec::new(),
                    })
                    .encode(&mut server_stream, first.serial)
                    .context("encode first response")?;
                    Write::flush(&mut server_stream).context("flush reverse-order responses")?;

                    let final_ping =
                        Pdu::decode(&mut server_stream).context("decode final live Ping")?;
                    anyhow::ensure!(
                        final_ping.serial == 5,
                        "same transport did not retain its serial high-water mark"
                    );
                    anyhow::ensure!(
                        matches!(final_ping.pdu, Pdu::Ping(_)),
                        "unexpected final request"
                    );
                    Pdu::Pong(Pong {})
                        .encode(&mut server_stream, final_ping.serial)
                        .context("encode final Pong")?;
                    Write::flush(&mut server_stream).context("flush final Pong")?;

                    release_server_rx
                        .recv_timeout(Duration::from_secs(10))
                        .context("hold transport through client assertions")?;
                    Ok(())
                })();
                let _ = server_done_tx.send(());
                result
            })
            .expect("spawn socket-pair RPC server");

        let unix_domain = UnixDomain {
            name: "ft-rpc-abandonment".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(unix_domain),
            Some(Box::new(client_stream)),
        );
        let client = Client::new(None, reconnectable, Weak::new());
        let generation = client
            .rpc_transport
            .active_generation()
            .expect("abandonment test transport starts live");
        client.rpc_transport.lifecycle.lock().protocol = Some(
            RpcProtocolAuthority::established_for_test(generation, CODEC_VERSION),
        );

        let (preclosed_tx, preclosed_rx) = bounded(1);
        drop(preclosed_rx);
        asupersync_block_on(
            client
                .sender
                .send(client.test_reader_message(Pdu::Ping(Ping {}), preclosed_tx)),
        )
        .expect("queue preclosed request");

        let search_request = |pane_id| {
            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern: mux::pane::Pattern::CaseSensitiveString("needle".to_string()),
                range: 0..100,
                limit: Some(4),
            })
        };
        let (abandoned_one_tx, abandoned_one_rx) = bounded(1);
        let (live_two_tx, live_two_rx) = bounded(1);
        let (abandoned_three_tx, abandoned_three_rx) = bounded(1);
        let (live_four_tx, live_four_rx) = bounded(1);
        asupersync_block_on(async {
            client
                .sender
                .send(client.test_reader_message(search_request(11), abandoned_one_tx))
                .await?;
            client
                .sender
                .send(client.test_reader_message(Pdu::GetTlsCreds(GetTlsCreds {}), live_two_tx))
                .await?;
            client
                .sender
                .send(client.test_reader_message(search_request(33), abandoned_three_tx))
                .await?;
            client
                .sender
                .send(client.test_reader_message(Pdu::Ping(Ping {}), live_four_tx))
                .await?;
            Ok::<(), async_channel::SendError<ReaderMessage>>(())
        })
        .expect("queue mixed RPCs");

        requests_admitted_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server should confirm all requests reached the wire");
        drop(abandoned_one_rx);
        drop(abandoned_three_rx);
        release_replies_tx
            .send(())
            .expect("release reverse-order server replies");

        let fourth = recv_rpc_with_timeout(&live_four_rx, "fourth live RPC");
        let second = recv_rpc_with_timeout(&live_two_rx, "second live RPC");
        assert_eq!(fourth, Pdu::Pong(Pong {}));
        match second {
            Pdu::GetTlsCredsResponse(info) => {
                assert_eq!(info.ca_cert_pem, "ft-rpc-correlation-ca");
                assert_eq!(info.client_cert_pem, "ft-rpc-correlation-client");
            }
            other => panic!(
                "serial two received the wrong response type: {}",
                other.pdu_name()
            ),
        }

        let (final_tx, final_rx) = bounded(1);
        asupersync_block_on(
            client
                .sender
                .send(client.test_reader_message(Pdu::Ping(Ping {}), final_tx)),
        )
        .expect("queue final Ping");
        assert_eq!(
            recv_rpc_with_timeout(&final_rx, "final Ping"),
            Pdu::Pong(Pong {})
        );
        assert_eq!(
            client.connection_generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION,
            "caller abandonment must not retire the connection generation"
        );

        let generation = Arc::clone(&client.connection_generation);
        drop(client);
        let retirement_deadline = std::time::Instant::now() + Duration::from_secs(10);
        while generation.load(AtomicOrdering::Acquire) == INITIAL_CONNECTION_GENERATION {
            assert!(
                std::time::Instant::now() < retirement_deadline,
                "production reconnect loop did not retire the destroyed transport"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            generation.load(AtomicOrdering::Acquire),
            INITIAL_CONNECTION_GENERATION + 1,
            "client destruction must retire exactly one transport generation"
        );

        release_server_tx
            .send(())
            .expect("release server after transport retirement");
        server_done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC server did not reach its bounded terminal state");
        server
            .join()
            .expect("RPC server thread should join")
            .expect("RPC server should complete without protocol errors");
    }

    /// The v64 reliable pane-write request and exact-prefix ACK must make a real
    /// socket/reader round trip. This is intentionally not ignored: a unit-only
    /// responder can miss a request/response registration error in the physical
    /// reader path.
    #[cfg(unix)]
    #[test]
    fn reliable_pane_write_round_trips_real_reader() {
        let _wd = hang_watchdog(15, "remote pane write RPC round-trip", 96);

        let (client_stream, mut server_stream) =
            UnixStream::pair().expect("create pane-write socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound pane-write server reads");
        server_stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("bound pane-write server writes");

        let server = std::thread::Builder::new()
            .name("ft-pane-write-server".to_string())
            .spawn(move || -> anyhow::Result<()> {
                loop {
                    let decoded =
                        Pdu::decode(&mut server_stream).context("server decode client PDU")?;
                    let (response, done) = match decoded.pdu {
                        Pdu::GetCodecVersion(_) => (
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "ft-pane-write-server".to_string(),
                                executable_path: PathBuf::from("/usr/local/bin/ft"),
                                config_file_path: None,
                                min_supported: CODEC_VERSION,
                            }),
                            false,
                        ),
                        Pdu::SetClientId(_) => (Pdu::UnitResponse(UnitResponse {}), false),
                        Pdu::ReliablePaneWriteV1(request) => (
                            Pdu::ReliablePaneWriteV1Response(ReliablePaneWriteV1Response {
                                pane_id: request.pane_id,
                                input_serial: request.input_serial,
                                outcome: ReliablePaneWriteOutcomeV1::AppliedPrefix {
                                    bytes: u32::try_from(request.data.len())
                                        .expect("bounded pane-write payload fits u32"),
                                },
                            }),
                            true,
                        ),
                        other => panic!("unexpected client PDU: {}", other.pdu_name()),
                    };
                    response
                        .encode(&mut server_stream, decoded.serial)
                        .context("server encode response PDU")?;
                    Write::flush(&mut server_stream).context("server flush response PDU")?;
                    if done {
                        break;
                    }
                }
                Ok(())
            })
            .expect("spawn pane-write UDS server");

        let ui = ConnectionUI::new_headless();
        let client_domain_config = ClientDomainConfig::Unix(UnixDomain {
            name: "ft-pane-write".to_string(),
            no_serve_automatically: true,
            read_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        let mut reconnectable =
            Reconnectable::new(client_domain_config.clone(), Some(Box::new(client_stream)));
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable();
        let is_local = reconnectable.is_local();
        let (sender, receiver) = unbounded();
        let client = std::sync::Arc::new(Client {
            sender,
            local_domain_id: None,
            incarnation: Arc::new(ClientIncarnation),
            connection_generation: Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            rpc_transport: Arc::new(RpcTransportState::new()),
            domain_reconnect_authorized: Arc::new(AtomicBool::new(true)),
            client_id: ClientId::new(),
            client_domain_config,
            is_reconnectable,
            is_local,
        });
        let dispatch_authority = client.test_dispatch_authority(Weak::new());

        let reader = std::thread::Builder::new()
            .name("ft-pane-write-reader".to_string())
            .spawn(move || {
                let (result, _reconnectable, _receiver) =
                    client_thread(reconnectable, receiver, dispatch_authority);
                result
            })
            .expect("spawn reader thread");

        asupersync_block_on(client.verify_version_compat(&ui)).expect("handshake completes");

        let request = ReliablePaneWriteV1 {
            pane_id: 1,
            pane_registration: Some(ReliablePaneRegistrationIdentityV1::from_bytes([0x45; 16])),
            input_serial: InputSerial::from_millis_since_epoch(73),
            data: b"hello-remote".to_vec(),
        };
        let write_client = std::sync::Arc::clone(&client);
        let result =
            asupersync_block_on(async move { write_client.reliable_pane_write_v1(request).await });
        let response = result.expect("reliable pane write must round-trip without panic or hang");
        assert_eq!(response.pane_id, 1);
        assert_eq!(response.input_serial.get(), 73);
        assert_eq!(
            response.outcome,
            ReliablePaneWriteOutcomeV1::AppliedPrefix { bytes: 12 }
        );

        drop(client);
        assert_expected_reader_shutdown(reader.join().expect("reader thread panicked"));
        server
            .join()
            .expect("server thread should join")
            .expect("server pane-write loop should succeed");
    }

    #[test]
    fn ssh_proxy_command_uses_override_verbatim() {
        let cmd = Reconnectable::build_ssh_proxy_command(
            &Some("/opt/wezterm".to_string()),
            Some("custom proxy --flag"),
            true,
        )
        .expect("build override command");
        assert_eq!(cmd, "custom proxy --flag");
    }

    #[test]
    fn ssh_proxy_command_uses_initial_proxy_launch_by_default() {
        let cmd = Reconnectable::build_ssh_proxy_command(&None, None, true)
            .expect("build initial proxy command");
        assert_eq!(cmd, "exec wezterm cli --prefer-mux proxy");
        assert!(!cmd.contains("/current/ft"));
        assert!(!cmd.contains("exec ft cli"));
    }

    #[test]
    fn ssh_proxy_command_disables_auto_start_on_reconnect() {
        let cmd = Reconnectable::build_ssh_proxy_command(&None, None, false)
            .expect("build reconnect proxy command");
        assert_eq!(cmd, "exec wezterm cli --prefer-mux --no-auto-start proxy");
        assert!(!cmd.contains("/current/ft"));
        assert!(!cmd.contains("exec ft cli"));
    }

    #[test]
    fn tls_creds_command_uses_remote_wezterm_path_when_present() {
        let cmd = Reconnectable::build_tls_creds_command(&Some("/usr/bin/wezterm".to_string()))
            .expect("build tls credentials command");
        assert_eq!(cmd, "exec /usr/bin/wezterm cli tlscreds");
    }

    #[test]
    fn tls_creds_command_defaults_to_the_implemented_wezterm_cli() {
        let cmd = Reconnectable::build_tls_creds_command(&None)
            .expect("build default TLS credentials command");
        assert_eq!(cmd, "exec wezterm cli tlscreds");
        assert!(!cmd.contains("/current/ft"));
        assert!(!cmd.contains("exec ft cli"));
    }

    #[test]
    fn explicit_remote_cli_path_is_shell_quoted() {
        let cmd = Reconnectable::build_ssh_proxy_command(
            &Some("/opt/Franken Term/wezterm;false".to_string()),
            None,
            true,
        )
        .expect("quote explicit remote executable");

        assert_eq!(
            cmd,
            "exec '/opt/Franken Term/wezterm;false' cli --prefer-mux proxy"
        );
    }

    #[test]
    fn explicit_remote_cli_path_rejects_nul() {
        let error =
            Reconnectable::build_tls_creds_command(&Some("/opt/wezterm\0suffix".to_string()))
                .expect_err("NUL cannot be represented in a remote shell command");

        assert!(
            error.to_string().contains("cannot be shell quoted"),
            "unexpected error: {}",
            error
        );
    }

    #[test]
    fn tls_bootstrap_retry_only_on_connection_refused() {
        let err = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(Reconnectable::should_retry_tls_bootstrap_after_reuse_error(
            &err
        ));
    }

    #[test]
    fn tls_bootstrap_retry_rejects_other_transport_failures() {
        let err = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));
        assert!(!Reconnectable::should_retry_tls_bootstrap_after_reuse_error(&err));
    }

    #[test]
    fn tls_bootstrap_retry_preserves_non_io_fallback_behavior() {
        let err = anyhow!("boom");
        assert!(Reconnectable::should_retry_tls_bootstrap_after_reuse_error(
            &err
        ));
    }

    #[test]
    fn ssh_registration_lock_recovers_after_poison() {
        let registration = Mutex::new(None::<IoRegistration>);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registration.lock().unwrap();
            panic!("simulate SSH registration lock poison");
        }));

        assert!(poisoned.is_err());
        assert!(registration.is_poisoned());

        {
            let guard = lock_registration_mutex(&registration);
            assert!(guard.is_none());
        }

        assert!(!registration.is_poisoned());
    }

    #[test]
    fn unix_connect_with_retry_rejects_zero_attempts() {
        let socket_path = PathBuf::from("/tmp/frankenterm-zero-attempts.sock");
        let err = match unix_connect_with_retry(&UnixTarget::Socket(socket_path), false, Some(0)) {
            Ok(_) => panic!("zero retry attempts should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("greater than zero"),
            "unexpected error: {:?}",
            err
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_proxy_transport_terminates_and_reaps_its_exact_child() {
        let (stream, _peer) = UnixStream::pair().expect("create proxy lifecycle socketpair");
        let child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn proxy-child lifecycle probe");
        let pid = child.id();

        drop(UnixConnectStream::proxy(stream, child));

        let probe = std::process::Command::new("sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .expect("probe exact proxy-child pid after transport drop");
        assert!(
            !probe.success(),
            "the transport drop must leave no live or zombie proxy child {}",
            pid
        );
    }

    #[test]
    fn unix_stream_connect_timeout_rejects_zero_deadline() {
        let err = unix_stream_connect_with_timeout(
            PathBuf::from("/tmp/frankenterm-zero-connect-timeout.sock"),
            Duration::ZERO,
        )
        .expect_err("zero connect timeout should fail before attempting to connect");

        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            err.to_string().contains("greater than zero"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn unix_connect_with_retry_rejects_empty_proxy_command() {
        let err = match unix_connect_with_retry(&UnixTarget::Proxy(Vec::new()), false, Some(1)) {
            Ok(_) => panic!("empty proxy command should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("proxy command is empty"),
            "unexpected error: {:?}",
            err
        );
    }

    #[test]
    fn unix_connect_rejects_empty_serve_command() {
        let socket_path = PathBuf::from("/tmp/frankenterm-empty-serve-command.sock");
        let unix_domain = UnixDomain {
            name: "empty-serve-command".to_string(),
            socket_path: Some(socket_path),
            serve_command: Some(Vec::new()),
            no_serve_automatically: false,
            ..Default::default()
        };
        let mut reconnectable =
            Reconnectable::new(ClientDomainConfig::Unix(unix_domain.clone()), None);
        let mut ui = ConnectionUI::new_headless();

        let err = reconnectable
            .unix_connect(unix_domain, true, &mut ui, false)
            .expect_err("empty serve command should fail before spawning");

        assert!(
            err.to_string().contains("serve command is empty"),
            "unexpected error: {:?}",
            err
        );
    }

    fn unilateral(pdu: Pdu) -> DecodedPdu {
        DecodedPdu { serial: 0, pdu }
    }

    fn coherent_snapshot_response(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: u64,
    ) -> Pdu {
        Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id,
            outcome: ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                session_incarnation,
                snapshot_revision: TopologyRevision::new(snapshot_revision),
                panes: ListPanesResponse {
                    tabs: Vec::new(),
                    tab_titles: Vec::new(),
                    window_titles: HashMap::new(),
                    floating_panes: Vec::new(),
                },
            }),
        })
    }

    fn same_numeric_id_snapshot_response(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: u64,
        label: &str,
    ) -> Pdu {
        Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id,
            outcome: ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                session_incarnation,
                snapshot_revision: TopologyRevision::new(snapshot_revision),
                panes: ListPanesResponse {
                    tabs: vec![PaneNode::Leaf(PaneEntry {
                        window_id: 41,
                        tab_id: 51,
                        pane_id: 61,
                        title: format!("{label} pane"),
                        size: TerminalSize {
                            cols: 120,
                            rows: 40,
                            pixel_width: 1_200,
                            pixel_height: 800,
                            dpi: 96,
                        },
                        working_dir: None,
                        alt_screen_active: false,
                        is_active_pane: true,
                        is_zoomed_pane: false,
                        workspace: "ops".to_string(),
                        cursor_pos: mux::renderable::StableCursorPosition::default(),
                        physical_top: 0,
                        top_row: 0,
                        left_col: 0,
                        tty_name: None,
                    })],
                    tab_titles: vec![format!("{label} tab")],
                    window_titles: HashMap::from([(41, format!("{label} window"))]),
                    floating_panes: Vec::new(),
                },
            }),
        })
    }

    fn stamped_title_event(stream_id: TopologyStreamId, revision: u64, title: &str) -> DecodedPdu {
        unilateral(Pdu::TopologyEvent(TopologyEvent {
            stream_id,
            revision: TopologyRevision::new(revision),
            event: TopologyEventKind::TabTitleChanged {
                tab_id: 2,
                title: title.to_string(),
            },
        }))
    }

    fn established_topology_coordinator(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: u64,
    ) -> (ClientTopologyCoordinator, TopologyFenceAuthority) {
        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(1).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("initial coherent fence should admit");
        let response =
            coherent_snapshot_response(stream_id, session_incarnation, snapshot_revision);
        assert!(matches!(
            coordinator
                .on_response(serial, &response)
                .expect("coherent response should await consumer commit"),
            ClientTopologyResponseAction::AwaitCommit
        ));
        let authority = TopologyFenceAuthority {
            stream_id,
            session_incarnation,
            snapshot_revision: TopologyRevision::new(snapshot_revision),
        };
        assert!(coordinator
            .commit(authority)
            .expect("initial coherent snapshot should commit")
            .is_empty());
        (coordinator, authority)
    }

    #[test]
    fn mux_rpc_bootstrap_deadline_cancels_a_stalled_stage() {
        let error = asupersync_block_on(with_mux_rpc_bootstrap_timeout_for(
            Duration::from_millis(5),
            futures::future::pending::<anyhow::Result<()>>(),
        ))
        .expect_err("a stalled bootstrap stage must have one finite deadline");

        assert!(error.root_cause().is::<Timeout>());
        assert!(error.to_string().contains("bootstrap exceeded"));
    }

    #[test]
    fn pre_ready_quarantine_is_count_and_byte_bounded_including_replay() {
        let mut queue = PreReadyUnilateralQueue::default();
        queue
            .enqueue_with_limits(
                unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 1,
                    title: "one".to_string(),
                })),
                0,
                0,
                2,
                1_024,
            )
            .expect("first small PDU fits");

        let count_error = queue
            .enqueue_with_limits(
                unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 1,
                    title: "blocked-by-in-flight-count".to_string(),
                })),
                1,
                0,
                2,
                1_024,
            )
            .expect_err("waiting plus in-flight PDUs must share one count budget");
        assert!(count_error.to_string().contains("2 PDU limit"));
        assert_eq!(queue.waiting.len(), 1);

        let oversized = Pdu::SetClipboard(SetClipboard {
            pane_id: 7,
            clipboard: Some("x".repeat(128)),
            selection: wezterm_term::ClipboardSelection::Clipboard,
        });
        let encoded_bytes = oversized
            .encode_retained_frame(0)
            .expect("measure the exact retained frame")
            .len();
        let mut byte_queue = PreReadyUnilateralQueue::default();
        let byte_error = byte_queue
            .enqueue_with_limits(unilateral(oversized), 0, 1, 8, encoded_bytes)
            .expect_err("in-flight and waiting frames must share one byte budget");
        assert!(byte_error.to_string().contains("byte limit"));
        assert!(byte_queue.waiting.is_empty());
        assert_eq!(byte_queue.waiting_bytes, 0);
    }

    #[test]
    fn legacy_snapshot_fence_replays_unilateral_events_in_arrival_order_after_commit() {
        let generation = NonZeroU64::new(9).expect("test generation is nonzero");
        let serial = NonZeroU64::new(17).expect("test serial is nonzero");
        let mut coordinator = ClientTopologyCoordinator::default();
        coordinator
            .begin_legacy_fence(serial)
            .expect("codec-46 local topology fence should admit");

        for decoded in [
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 41 })),
            unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                window_id: 7,
                title: "before-response".to_string(),
            })),
        ] {
            assert!(matches!(
                coordinator
                    .on_unilateral(decoded)
                    .expect("pre-response codec-46 event should quarantine"),
                ClientTopologyUnilateralAction::Buffered
            ));
        }
        coordinator
            .on_legacy_response(serial)
            .expect("matching codec-46 response should await consumer commit");
        assert!(matches!(
            coordinator
                .on_unilateral(unilateral(Pdu::TabTitleChanged(TabTitleChanged {
                    tab_id: 8,
                    title: "after-response".to_string(),
                })))
                .expect("post-response codec-46 event should remain quarantined"),
            ClientTopologyUnilateralAction::Buffered
        ));

        let routed = coordinator
            .commit_legacy(LegacyTopologyFenceAuthority { generation, serial })
            .expect("exact consumer commit should release all codec-46 events");
        assert_eq!(routed.len(), 3);
        assert!(matches!(
            routed[0].pdu,
            Pdu::PaneRemoved(PaneRemoved { pane_id: 41 })
        ));
        assert!(matches!(
            &routed[1].pdu,
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id: 7, title })
                if title == "before-response"
        ));
        assert!(matches!(
            &routed[2].pdu,
            Pdu::TabTitleChanged(TabTitleChanged { tab_id: 8, title })
                if title == "after-response"
        ));
        assert!(matches!(coordinator.phase, ClientTopologyPhase::Legacy));
    }

    #[test]
    fn coherent_snapshot_prunes_only_matching_revisions_after_consumer_commit() {
        let stream_id = TopologyStreamId::from_bytes([0x51; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa1; 16]);
        let authority = TopologyFenceAuthority {
            stream_id,
            session_incarnation,
            snapshot_revision: TopologyRevision::new(5),
        };
        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(1).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("coherent fence should admit");
        assert!(matches!(
            coordinator
                .on_unilateral(unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 1 })))
                .expect("legacy event should quarantine"),
            ClientTopologyUnilateralAction::Buffered
        ));

        let response = coherent_snapshot_response(stream_id, session_incarnation, 5);
        assert!(matches!(
            coordinator
                .on_response(serial, &response)
                .expect("snapshot response should establish pending authority"),
            ClientTopologyResponseAction::AwaitCommit
        ));
        for (revision, title) in [(5, "subsumed"), (6, "replayed")] {
            assert!(matches!(
                coordinator
                    .on_unilateral(stamped_title_event(stream_id, revision, title))
                    .expect("matching stamped event should quarantine"),
                ClientTopologyUnilateralAction::Buffered
            ));
        }

        let routed = coordinator
            .commit(authority)
            .expect("exact consumer commit should establish the stream");
        assert_eq!(routed.len(), 1);
        assert!(matches!(
            &routed[0].pdu,
            Pdu::TabTitleChanged(TabTitleChanged { tab_id: 2, title })
                if title == "replayed"
        ));
        assert!(matches!(
            coordinator.phase,
            ClientTopologyPhase::Established(_)
        ));
    }

    #[test]
    fn coherent_snapshot_contention_restores_legacy_events_without_pruning() {
        let stream_id = TopologyStreamId::from_bytes([0x52; 16]);
        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(2).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("coherent fence should admit");
        coordinator
            .on_unilateral(unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 91 })))
            .expect("legacy event should quarantine");

        let response = Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id,
            outcome: ListPanesCoherentOutcome::Contended {
                attempts: 3,
                first_revision: TopologyRevision::new(7),
                last_revision: TopologyRevision::new(9),
            },
        });
        let ClientTopologyResponseAction::Route(routed) = coordinator
            .on_response(serial, &response)
            .expect("typed contention should restore the prior stream")
        else {
            panic!("typed contention must route the quarantined legacy event");
        };
        assert_eq!(routed.len(), 1);
        assert!(matches!(
            routed[0].pdu,
            Pdu::PaneRemoved(PaneRemoved { pane_id: 91 })
        ));
        assert!(matches!(coordinator.phase, ClientTopologyPhase::Legacy));
    }

    #[test]
    fn rejected_snapshot_closes_without_authorizing_event_pruning() {
        let stream_id = TopologyStreamId::from_bytes([0x53; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa3; 16]);
        let authority = TopologyFenceAuthority {
            stream_id,
            session_incarnation,
            snapshot_revision: TopologyRevision::new(12),
        };
        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(3).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("coherent fence should admit");
        let response = coherent_snapshot_response(stream_id, session_incarnation, 12);
        coordinator
            .on_response(serial, &response)
            .expect("snapshot should await a decision");
        coordinator
            .on_unilateral(stamped_title_event(stream_id, 12, "must-not-be-pruned"))
            .expect("matching event should remain quarantined");

        coordinator
            .reject(authority)
            .expect("the exact consumer may reject its delivered snapshot");
        assert!(matches!(coordinator.phase, ClientTopologyPhase::Closed));
        let error = coordinator
            .on_unilateral(stamped_title_event(stream_id, 13, "too-late"))
            .expect_err("rejection makes the connection topology stream terminal");
        assert!(error.to_string().contains("closed"));
    }

    #[test]
    fn rejected_snapshot_ack_is_published_only_after_transport_revocation() {
        let stream_id = TopologyStreamId::from_bytes([0x54; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa4; 16]);
        let authority = TopologyFenceAuthority {
            stream_id,
            session_incarnation,
            snapshot_revision: TopologyRevision::new(13),
        };
        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(4).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("coherent fence should admit");
        coordinator
            .on_response(
                serial,
                &coherent_snapshot_response(stream_id, session_incarnation, 13),
            )
            .expect("snapshot should await a decision");

        let (client, _receiver) = client_with_idle_rpc_queue();
        let dispatch_authority = client.test_dispatch_authority(Weak::new());
        let generation = dispatch_authority
            .rpc_transport
            .active_generation()
            .expect("test transport generation is live");
        let (promise, acknowledgement_rx): (
            Sender<anyhow::Result<TopologySnapshotDecisionAck>>,
            Receiver<anyhow::Result<TopologySnapshotDecisionAck>>,
        ) = bounded(1);
        let acknowledgement = coordinator
            .reject_and_retire_transport(authority, &dispatch_authority)
            .expect("snapshot rejection should retire its exact transport");
        promise
            .try_send(Ok(acknowledgement))
            .expect("publish typed terminal acknowledgement");

        assert_eq!(
            acknowledgement_rx
                .try_recv()
                .expect("consumer should observe the terminal acknowledgement")
                .expect("terminal acknowledgement should be successful"),
            TopologySnapshotDecisionAck::RejectedTerminal
        );
        assert_eq!(
            dispatch_authority
                .rpc_transport
                .live_generation
                .load(AtomicOrdering::Acquire),
            0,
            "terminal acknowledgement must not become observable while RPC admission is live"
        );
        assert!(matches!(
            dispatch_authority.rpc_transport.lifecycle.lock().phase,
            RpcTransportPhase::Reconnecting { retired, .. } if retired == generation
        ));
    }

    #[test]
    fn snapshot_decision_drop_revokes_exact_generation() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let generation = rpc_transport
            .active_generation()
            .expect("test generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test generation has reader abort authority");
        drop(TopologySnapshotDecisionGuard::new(
            Arc::clone(&rpc_transport),
            Arc::clone(&reader_abort),
        ));

        assert_eq!(
            reader_abort.cause(),
            Some("coherent topology snapshot cancelled after response delivery")
        );
        assert_eq!(
            rpc_transport.live_generation.load(AtomicOrdering::Acquire),
            0
        );
    }

    #[test]
    fn delivered_snapshot_cancellation_never_prunes_or_double_settles_pending_state() {
        let stream_id = TopologyStreamId::from_bytes([0x64; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xb4; 16]);
        let authority = TopologyFenceAuthority {
            stream_id,
            session_incarnation,
            snapshot_revision: TopologyRevision::new(17),
        };
        let (mut pending, probe) = pending_replies_for_test();
        let generation = pending.generation;
        let (completion_tx, completion_rx) = bounded(1);
        let serial = pending
            .admit_named_expect(
                completion_tx,
                "ListPanesCoherent",
                Some(test_wire_ident::<ListPanesCoherentResponse>()),
            )
            .expect("admit coherent topology request")
            .expect("assign coherent topology serial");
        pending
            .map
            .get_mut(&serial)
            .expect("coherent topology request must remain pending")
            .effect = PendingRpcEffect::CoherentTopologyFence;

        let mut topology = ClientTopologyCoordinator::default();
        topology
            .begin_fence(serial)
            .expect("coherent topology fence should begin");
        let response = coherent_snapshot_response(stream_id, session_incarnation, 17);
        let frame = response
            .encode_frame_with_mode(serial.get(), CompressionMode::Never)
            .expect("encode coherent response transcript");
        let decoded =
            Pdu::decode(std::io::Cursor::new(frame)).expect("decode coherent response transcript");
        assert_eq!(decoded.serial, serial.get());
        assert!(matches!(
            topology
                .on_response(serial, &decoded.pdu)
                .expect("decoded response must await its exact consumer"),
            ClientTopologyResponseAction::AwaitCommit
        ));
        assert_eq!(
            pending
                .complete(serial, PendingRpcReply::pdu(decoded.pdu))
                .expect("decoded coherent response must settle its pending RPC"),
            ReplyCompletion {
                disposition: ReplyDisposition::Delivered,
            }
        );
        assert!(matches!(
            completion_rx
                .try_recv()
                .expect("consumer must observe the delivered coherent response"),
            Ok(PendingRpcReply::Pdu(pdu))
                if matches!(pdu.as_ref(), Pdu::ListPanesCoherentResponse(_))
        ));

        assert!(matches!(
            topology
                .on_unilateral(stamped_title_event(
                    stream_id,
                    authority.snapshot_revision.get(),
                    "must-survive-until-terminal-rejection",
                ))
                .expect("post-response event must remain retained until a decision"),
            ClientTopologyUnilateralAction::Buffered
        ));
        let ClientTopologyPhase::AwaitingCommit(awaiting) = &topology.phase else {
            panic!("delivered snapshot must still await its exact consumer commit");
        };
        assert!(
            awaiting
                .events
                .events
                .contains_key(&authority.snapshot_revision),
            "consumer cancellation must not pre-prune a snapshot-covered event"
        );
        assert_eq!(
            pending
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0,
            "delivering a snapshot is not readiness authority"
        );

        let reader_abort = pending
            .rpc_transport
            .reader_abort_for(generation)
            .expect("pending topology response has reader abort authority");
        drop(TopologySnapshotDecisionGuard::new(
            Arc::clone(&pending.rpc_transport),
            Arc::clone(&reader_abort),
        ));
        assert_eq!(
            reader_abort.cause(),
            Some("coherent topology snapshot cancelled after response delivery")
        );
        let ClientTopologyPhase::AwaitingCommit(awaiting) = &topology.phase else {
            panic!("reader-local topology state must remain unpruned until teardown");
        };
        assert!(
            awaiting
                .events
                .events
                .contains_key(&authority.snapshot_revision),
            "direct cancellation must not authorize snapshot-covered pruning"
        );
        drop(topology);

        assert!(pending.map.is_empty());
        assert_eq!(probe.pending(), 0.0);
        assert_eq!(RpcMetricProbe::counter(&probe.admitted), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
        assert_eq!(RpcMetricProbe::counter(&probe.abandoned), 0);
        assert_eq!(RpcMetricProbe::counter(&probe.transport_failed_live), 0);
        assert_eq!(
            RpcMetricProbe::counter(&probe.transport_cleared_abandoned),
            0
        );
        pending.fail_all("terminal cancellation after coherent response delivery");
        drop(pending);
        probe.assert_balanced();
        assert_eq!(RpcMetricProbe::counter(&probe.delivered), 1);
    }

    #[test]
    fn snapshot_request_drop_aborts_its_exact_generation() {
        let rpc_transport = Arc::new(RpcTransportState::new());
        let generation = rpc_transport
            .active_generation()
            .expect("test generation is live");
        let reader_abort = rpc_transport
            .reader_abort_for(generation)
            .expect("test generation has reader abort authority");
        drop(TopologySnapshotRequestGuard::new(
            Arc::clone(&rpc_transport),
            Arc::clone(&reader_abort),
        ));

        assert_eq!(
            reader_abort.cause(),
            Some("coherent topology snapshot cancelled before exact consumer decision")
        );
        assert_eq!(
            rpc_transport.live_generation.load(AtomicOrdering::Acquire),
            0
        );
    }

    #[test]
    fn established_topology_stream_rejects_wrong_duplicate_and_gapped_revisions() {
        let stream_id = TopologyStreamId::from_bytes([0x55; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa5; 16]);

        let (mut wrong_stream, _) =
            established_topology_coordinator(stream_id, session_incarnation, 5);
        let wrong_error = wrong_stream
            .on_unilateral(stamped_title_event(
                TopologyStreamId::from_bytes([0x56; 16]),
                6,
                "wrong-stream",
            ))
            .expect_err("a different stream identity must fail closed");
        assert!(wrong_error.to_string().contains("wrong established stream"));

        let (mut duplicate, _) =
            established_topology_coordinator(stream_id, session_incarnation, 5);
        let ClientTopologyUnilateralAction::Route(first) = duplicate
            .on_unilateral(stamped_title_event(stream_id, 6, "first"))
            .expect("the exact successor should route")
        else {
            panic!("the exact successor must route immediately");
        };
        assert_eq!(first.len(), 1);
        let duplicate_error = duplicate
            .on_unilateral(stamped_title_event(stream_id, 6, "duplicate"))
            .expect_err("a repeated revision must fail closed");
        assert!(duplicate_error.to_string().contains("stale or duplicate"));

        let (mut gap, _) = established_topology_coordinator(stream_id, session_incarnation, 5);
        let gap_error = gap
            .on_unilateral(stamped_title_event(stream_id, 7, "gap"))
            .expect_err("a missing immediate successor must fail closed");
        assert!(gap_error.to_string().contains("lost revision 6"));
    }

    #[test]
    fn malformed_topology_events_and_mixed_version_fences_fail_closed() {
        let stream_id = TopologyStreamId::from_bytes([0x57; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa7; 16]);

        for (event_stream, revision, expected) in [
            (
                TopologyStreamId::from_bytes([0; 16]),
                1,
                "zero stream identity",
            ),
            (stream_id, 0, "initial snapshot-only revision"),
            (stream_id, u64::MAX, "exhausted terminal revision"),
        ] {
            let mut coordinator = ClientTopologyCoordinator::default();
            let error = coordinator
                .on_unilateral(stamped_title_event(event_stream, revision, "malformed"))
                .expect_err("reserved topology authority must be rejected");
            assert!(
                error.to_string().contains(expected),
                "unexpected malformed-event error: {:#}",
                error,
            );
        }

        let mut coordinator = ClientTopologyCoordinator::default();
        let serial = NonZeroU64::new(4).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("coherent fence should admit");
        let mut response = match coherent_snapshot_response(stream_id, session_incarnation, 5) {
            Pdu::ListPanesCoherentResponse(response) => response,
            _ => unreachable!("helper always returns a coherent response"),
        };
        response.negotiated = TopologyCapabilities::NONE;
        let error = coordinator
            .on_response(serial, &Pdu::ListPanesCoherentResponse(response))
            .expect_err("a legacy or partially negotiated peer must fail closed");
        assert!(error.to_string().contains("unexpected capability bits"));
    }

    #[test]
    fn coherent_resnapshot_cannot_regress_committed_authority() {
        let stream_id = TopologyStreamId::from_bytes([0x58; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa8; 16]);
        let (mut coordinator, _) =
            established_topology_coordinator(stream_id, session_incarnation, 8);
        let serial = NonZeroU64::new(5).expect("test serial is nonzero");
        coordinator
            .begin_fence(serial)
            .expect("resnapshot fence should admit");
        let response = coherent_snapshot_response(stream_id, session_incarnation, 7);
        let error = coordinator
            .on_response(serial, &response)
            .expect_err("a regressing snapshot must fail closed");
        assert!(error.to_string().contains("regressed behind committed"));
    }

    #[test]
    fn structural_topology_event_coalesces_one_exact_resync_trigger() {
        let stream_id = TopologyStreamId::from_bytes([0x59; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa9; 16]);
        let (mut coordinator, _) =
            established_topology_coordinator(stream_id, session_incarnation, 5);
        let action = coordinator
            .on_unilateral(unilateral(Pdu::TopologyEvent(TopologyEvent {
                stream_id,
                revision: TopologyRevision::new(6),
                event: TopologyEventKind::WindowCreated { window_id: 31 },
            })))
            .expect("the exact structural successor should route");
        let ClientTopologyUnilateralAction::Route(routed) = action else {
            panic!("the structural successor must route a resync trigger");
        };
        assert_eq!(routed.len(), 1);
        assert!(matches!(
            routed[0].pdu,
            Pdu::TabResized(codec::TabResized { tab_id: 0 })
        ));
    }

    #[test]
    fn reconnect_uses_a_fresh_topology_coordinator_and_stream_identity() {
        let first_stream = TopologyStreamId::from_bytes([0x5a; 16]);
        let second_stream = TopologyStreamId::from_bytes([0x5b; 16]);
        let first_session = MuxSessionIncarnation::from_bytes([0xaa; 16]);
        let second_session = MuxSessionIncarnation::from_bytes([0xab; 16]);
        let (first, first_authority) =
            established_topology_coordinator(first_stream, first_session, 11);
        let (second, second_authority) =
            established_topology_coordinator(second_stream, second_session, 0);

        assert!(matches!(first.phase, ClientTopologyPhase::Established(_)));
        assert!(matches!(second.phase, ClientTopologyPhase::Established(_)));
        assert_ne!(first_authority.stream_id, second_authority.stream_id);
        assert_ne!(
            first_authority.session_incarnation,
            second_authority.session_incarnation
        );
    }

    #[test]
    fn reused_numeric_ids_cannot_carry_stale_topology_into_a_new_session_incarnation() {
        let old_stream = TopologyStreamId::from_bytes([0x6a; 16]);
        let new_stream = TopologyStreamId::from_bytes([0x6b; 16]);
        let old_session = MuxSessionIncarnation::from_bytes([0xba; 16]);
        let new_session = MuxSessionIncarnation::from_bytes([0xbb; 16]);
        let old_snapshot = same_numeric_id_snapshot_response(old_stream, old_session, 0, "old");
        let new_snapshot = same_numeric_id_snapshot_response(new_stream, new_session, 0, "new");

        let numeric_ids = |response: &Pdu| {
            let Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                outcome: ListPanesCoherentOutcome::Snapshot(snapshot),
                ..
            }) = response
            else {
                panic!("same-ID helper must return a coherent snapshot");
            };
            let [PaneNode::Leaf(entry)] = snapshot.panes.tabs.as_slice() else {
                panic!("same-ID helper must return one leaf pane");
            };
            (entry.window_id, entry.tab_id, entry.pane_id)
        };
        assert_eq!(numeric_ids(&old_snapshot), (41, 51, 61));
        assert_eq!(numeric_ids(&new_snapshot), (41, 51, 61));

        let establish = |serial: u64, response: &Pdu| {
            let Pdu::ListPanesCoherentResponse(response_body) = response else {
                panic!("establishment requires a coherent response");
            };
            let authority = TopologyFenceAuthority::from_response(response_body)
                .expect("snapshot must carry valid authority");
            let mut coordinator = ClientTopologyCoordinator::default();
            let serial = NonZeroU64::new(serial).expect("test serial is nonzero");
            coordinator
                .begin_fence(serial)
                .expect("initial coherent fence should admit");
            assert!(matches!(
                coordinator
                    .on_response(serial, response)
                    .expect("snapshot should await its exact commit"),
                ClientTopologyResponseAction::AwaitCommit
            ));
            assert!(coordinator
                .commit(authority)
                .expect("snapshot should establish its connection-scoped stream")
                .is_empty());
            (coordinator, authority)
        };
        let (mut old_topology, old_authority) = establish(1, &old_snapshot);
        let (mut new_topology, new_authority) = establish(1, &new_snapshot);
        assert_ne!(old_authority.stream_id, new_authority.stream_id);
        assert_ne!(
            old_authority.session_incarnation,
            new_authority.session_incarnation
        );

        let (client, rpc_queue) = client_with_idle_rpc_queue();
        let retired_dispatch = client.test_dispatch_authority(Weak::new());
        let successor_dispatch = retired_dispatch
            .advance_generation(&rpc_queue)
            .expect("reconnect should mint one successor generation");
        successor_dispatch
            .activate_rpc_transport()
            .expect("successor generation should become live but remain unready");
        assert!(!retired_dispatch.generation_is_current());
        assert!(successor_dispatch.generation_is_current());
        assert_eq!(
            client
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0,
            "a new incarnation must not inherit readiness from the retired connection"
        );

        let new_fence_serial = NonZeroU64::new(2).expect("test serial is nonzero");
        new_topology
            .begin_fence(new_fence_serial)
            .expect("new incarnation should admit its own resnapshot");
        assert!(matches!(
            new_topology
                .on_unilateral(unilateral(Pdu::TopologyEvent(TopologyEvent {
                    stream_id: new_stream,
                    revision: TopologyRevision::new(2),
                    event: TopologyEventKind::PaneFocused { pane_id: 61 },
                })))
                .expect("new-stream event should remain buffered behind its missing predecessor"),
            ClientTopologyUnilateralAction::Buffered
        ));

        let new_state = |coordinator: &ClientTopologyCoordinator| {
            let ClientTopologyPhase::Fencing(ClientTopologyFenceInFlight {
                prior: ClientTopologyPrior::Established(established),
                ..
            }) = &coordinator.phase
            else {
                panic!("new incarnation must retain its established stream behind the fence");
            };
            let retained = established
                .events
                .events
                .get(&TopologyRevision::new(2))
                .expect("new incarnation must retain its revision-2 event");
            assert!(matches!(
                &retained.event.event,
                TopologyEventKind::PaneFocused { pane_id: 61 }
            ));
            (
                established.authority,
                established.next_revision,
                established.events.events.len(),
                established.events.retained_bytes,
            )
        };
        let before_stale_delivery = new_state(&new_topology);

        let late_old_serial = NonZeroU64::new(2).expect("test serial is nonzero");
        old_topology
            .begin_fence(late_old_serial)
            .expect("retired reader owns its own late resnapshot fence");
        let late_old_snapshot =
            same_numeric_id_snapshot_response(old_stream, old_session, 1, "late-old");
        assert!(matches!(
            old_topology
                .on_response(late_old_serial, &late_old_snapshot)
                .expect("late old snapshot remains confined to the retired coordinator"),
            ClientTopologyResponseAction::AwaitCommit
        ));
        let late_old_authority = match &late_old_snapshot {
            Pdu::ListPanesCoherentResponse(response) => {
                TopologyFenceAuthority::from_response(response)
                    .expect("late old snapshot authority should remain well formed")
            }
            _ => unreachable!("same-ID helper always returns a coherent response"),
        };
        assert!(old_topology
            .commit(late_old_authority)
            .expect("retired coordinator may settle only its own snapshot")
            .is_empty());
        assert_eq!(
            new_state(&new_topology),
            before_stale_delivery,
            "a late old-session snapshot must not mutate or prune successor state"
        );

        let stale_event_error = new_topology
            .on_unilateral(unilateral(Pdu::TopologyEvent(TopologyEvent {
                stream_id: old_stream,
                revision: TopologyRevision::new(2),
                event: TopologyEventKind::TabAddedToWindow {
                    tab_id: 51,
                    window_id: 41,
                },
            })))
            .expect_err("an old-session event must not target reused successor identifiers");
        assert!(stale_event_error
            .to_string()
            .contains("wrong established stream identity"));
        assert_eq!(
            new_state(&new_topology),
            before_stale_delivery,
            "a rejected stale event must not mutate or prune successor state"
        );
        process_unilateral(
            &retired_dispatch,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 61 })),
        )
        .expect("retired dispatch must discard queued work for the reused pane id");
        assert_eq!(
            client
                .rpc_transport
                .ready_generation
                .load(AtomicOrdering::Acquire),
            0,
            "stale same-ID work must never publish successor readiness"
        );
    }

    #[test]
    fn client_topology_retention_overflow_is_terminal_without_mutating_the_queue() {
        let stream_id = TopologyStreamId::from_bytes([0x5c; 16]);
        let mut events = ClientTopologyEventBuffer::default();
        events
            .insert_with_limits(
                match stamped_title_event(stream_id, 1, "retained").pdu {
                    Pdu::TopologyEvent(event) => event,
                    _ => unreachable!("helper always returns a stamped event"),
                },
                1,
                usize::MAX,
            )
            .expect("first event fits the injected bound");
        let retained_bytes = events.retained_bytes;
        let error = events
            .insert_with_limits(
                match stamped_title_event(stream_id, 2, "overflow").pdu {
                    Pdu::TopologyEvent(event) => event,
                    _ => unreachable!("helper always returns a stamped event"),
                },
                1,
                usize::MAX,
            )
            .expect_err("the second event must exceed the injected count bound");
        assert!(error.to_string().contains("above limits"));
        assert_eq!(events.events.len(), 1);
        assert_eq!(events.retained_bytes, retained_bytes);
        assert!(events.events.contains_key(&TopologyRevision::new(1)));
    }

    #[test]
    fn pre_ready_batch_accounting_errors_leave_the_exact_queue_unchanged() {
        fn retained_names(queue: &PreReadyUnilateralQueue) -> Vec<&'static str> {
            queue
                .waiting
                .iter()
                .map(|queued| {
                    Pdu::decode_retained_frame(queued.frame.as_slice())
                        .expect("decode retained notification")
                        .pdu
                        .pdu_name()
                })
                .collect()
        }

        let mut batch_queue = PreReadyUnilateralQueue::default();
        batch_queue
            .enqueue_with_limits(
                unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 3,
                    title: "still queued".to_string(),
                })),
                0,
                0,
                8,
                1_048_576,
            )
            .expect("enqueue replay notification");
        let batch_names = retained_names(&batch_queue);
        batch_queue.waiting_bytes = 0;
        let underflow = batch_queue
            .take_batch()
            .expect_err("batch underflow must fail before popping the frame");
        assert!(underflow.to_string().contains("accounting underflow"));
        assert_eq!(retained_names(&batch_queue), batch_names);
        assert_eq!(batch_queue.waiting.len(), 1);
        assert_eq!(batch_queue.waiting_bytes, 0);
    }

    fn standalone_dispatch_authority() -> ClientDispatchAuthority {
        ClientDispatchAuthority::new(
            None,
            Weak::new(),
            Arc::new(ClientIncarnation),
            Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            Arc::new(RpcTransportState::new()),
        )
    }

    #[test]
    fn standalone_client_ignores_cosmetic_unilateral_updates() {
        let authority = standalone_dispatch_authority();
        let result = process_unilateral(
            &authority,
            unilateral(Pdu::WindowTitleChanged(WindowTitleChanged {
                window_id: 3,
                title: "dev shell".into(),
            })),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn standalone_client_rejects_workspace_rebinding_without_domain() {
        let authority = standalone_dispatch_authority();
        let err = process_unilateral(
            &authority,
            unilateral(Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
                window_id: 7,
                workspace: "ops".into(),
            })),
        )
        .expect_err("workspace topology changes must not be silently ignored");
        let root = err
            .root_cause()
            .downcast_ref::<StandaloneUnilateralError>()
            .expect("root cause should preserve the standalone unilateral classification");
        assert_eq!(
            root,
            &StandaloneUnilateralError::RequiresAttachedDomain {
                pdu_name: "WindowWorkspaceChanged",
            }
        );
    }

    #[test]
    fn standalone_client_rejects_pane_removal_without_domain() {
        let authority = standalone_dispatch_authority();
        let err = process_unilateral(
            &authority,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect_err("pane removal changes must not be silently ignored");
        let root = err
            .root_cause()
            .downcast_ref::<StandaloneUnilateralError>()
            .expect("root cause should preserve the standalone unilateral classification");
        assert_eq!(
            root,
            &StandaloneUnilateralError::RequiresAttachedDomain {
                pdu_name: "PaneRemoved",
            }
        );
    }

    #[test]
    fn retired_connection_generation_discards_queued_unilateral_work() {
        let stale = standalone_dispatch_authority();
        let (_sender, receiver) = unbounded();
        let current = stale
            .advance_generation(&receiver)
            .expect("the successor transport generation should be minted");
        current
            .activate_rpc_transport()
            .expect("the successor transport should become live");

        assert!(!stale.generation_is_current());
        assert!(current.generation_is_current());
        process_unilateral(
            &stale,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect("a PDU queued by a retired transport must fail closed");
        process_unilateral(
            &current,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect_err("the current standalone transport retains topology classification");
    }

    #[test]
    fn generation_abort_revokes_unilateral_dispatch_before_reader_teardown() {
        let authority = standalone_dispatch_authority();
        let generation = authority
            .rpc_transport
            .active_generation()
            .expect("standalone test transport is live");
        let reader_abort = authority
            .rpc_transport
            .reader_abort_for(generation)
            .expect("live generation has exact reader abort authority");
        assert!(authority.generation_is_current());

        assert!(authority
            .rpc_transport
            .request_generation_abort(&reader_abort, "test cancellation before reader teardown",));
        assert!(
            authority.generation_is_current(),
            "identity authority must remain current so final detach can resolve its owner"
        );
        assert!(
            !authority.rpc_generation_is_live(),
            "revoked live authority must fence main-thread mutation before generation advance"
        );
        process_unilateral(
            &authority,
            unilateral(Pdu::PaneRemoved(PaneRemoved { pane_id: 9 })),
        )
        .expect("a unilateral queued before cancellation must be discarded after revocation");
    }

    #[test]
    fn client_incarnation_rejects_replacement_client_with_same_domain_id() {
        let config = ClientDomainConfig::Unix(UnixDomain::default());
        let old_client = Client::new_test_client(Some(42), config.clone());
        let replacement_client = Client::new_test_client(Some(42), config);
        let authority = old_client.test_dispatch_authority(Weak::new());

        assert!(old_client.matches_dispatch_authority(&authority));
        assert!(!replacement_client.matches_dispatch_authority(&authority));
    }

    #[test]
    fn attached_authority_uses_captured_mux_not_process_global_mux() {
        let scope = MuxTestScope::enter();
        let origin_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        scope.set_mux(&replacement_mux);

        let authority = ClientDispatchAuthority::new(
            Some(42),
            Arc::downgrade(&origin_mux),
            Arc::new(ClientIncarnation),
            Arc::new(AtomicU64::new(INITIAL_CONNECTION_GENERATION)),
            Arc::new(RpcTransportState::new()),
        );
        let captured = authority
            .captured_mux()
            .expect("captured mux owner should remain alive");
        assert!(Arc::ptr_eq(&captured, &origin_mux));
        assert!(!Arc::ptr_eq(&captured, &replacement_mux));
    }

    #[test]
    fn ssh_wait_for_readable_fails_closed_without_task_context() {
        let (stdin, remote_stdin) = filedescriptor::socketpair().expect("create stdin socketpair");
        let (remote_stdout, stdout) =
            filedescriptor::socketpair().expect("create stdout socketpair");
        let stream = SshStream::new(stdin, stdout).expect("construct ssh stream");
        let mut wait = Box::pin(stream.wait_for_readable());
        let waker = futures::task::noop_waker();
        let mut task_cx = TaskContext::from_waker(&waker);

        let Poll::Ready(Err(error)) = wait.as_mut().poll(&mut task_cx) else {
            panic!("missing task context must fail on the first readiness poll");
        };
        assert_eq!(error.kind(), ErrorKind::NotConnected);
        assert!(matches!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SshReadinessAuthorityError>()),
            Some(SshReadinessAuthorityError::MissingContext { operation: "read" })
        ));

        // Keep the idle peers alive through the poll so EOF cannot satisfy the
        // readiness probe before the missing-authority path is exercised.
        drop((remote_stdin, remote_stdout));
    }

    #[cfg(unix)]
    #[test]
    fn ssh_wait_for_readable_fails_closed_without_reactor() {
        let (stdin, remote_stdin) = filedescriptor::socketpair().expect("create stdin socketpair");
        let (remote_stdout, stdout) =
            filedescriptor::socketpair().expect("create stdout socketpair");
        let stream = SshStream::new(stdin, stdout).expect("construct ssh stream");
        let mut wait = Box::pin(stream.wait_for_readable());
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(false)
            .build()
            .expect("construct runtime without an I/O reactor");

        let first_poll =
            runtime.block_on(poll_fn(|task_cx| Poll::Ready(wait.as_mut().poll(task_cx))));
        let Poll::Ready(Err(error)) = first_poll else {
            panic!("missing reactor must fail on the first readiness poll");
        };
        assert_eq!(error.kind(), ErrorKind::NotConnected);
        assert!(matches!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<SshReadinessAuthorityError>()),
            Some(SshReadinessAuthorityError::ReactorUnavailable {
                operation: "read",
                phase: "register",
                ..
            })
        ));

        drop((remote_stdin, remote_stdout));
    }

    #[test]
    fn ssh_wait_for_readable_ignores_spurious_wake() {
        let (stdin, remote_stdin) = filedescriptor::socketpair().expect("create stdin socketpair");
        let (remote_stdout, stdout) =
            filedescriptor::socketpair().expect("create stdout socketpair");
        let stream = SshStream::new(stdin, stdout).expect("construct ssh stream");
        let mut wait = Box::pin(stream.wait_for_readable());

        asupersync_block_on(poll_fn(|task_cx| {
            assert!(
                wait.as_mut().poll(task_cx).is_pending(),
                "an idle descriptor must register and remain pending"
            );
            task_cx.waker().wake_by_ref();
            assert!(
                wait.as_mut().poll(task_cx).is_pending(),
                "an arbitrary wake must not be mistaken for descriptor readability"
            );
            Poll::Ready(())
        }));

        drop((remote_stdin, remote_stdout));
    }

    #[test]
    fn ssh_readiness_fleet_completes_only_the_readable_descriptor() {
        const FLEET_SIZE: usize = 8;
        const READY_INDEX: usize = 5;

        let _watchdog = hang_watchdog(12, "SSH readiness socketpair fleet", 91);
        let mut streams = Vec::with_capacity(FLEET_SIZE);
        let mut remote_stdin = Vec::with_capacity(FLEET_SIZE);
        let mut remote_stdout = Vec::with_capacity(FLEET_SIZE);
        for _ in 0..FLEET_SIZE {
            let (stdin, stdin_peer) =
                filedescriptor::socketpair().expect("create fleet stdin socketpair");
            let (stdout_peer, stdout) =
                filedescriptor::socketpair().expect("create fleet stdout socketpair");
            streams.push(SshStream::new(stdin, stdout).expect("construct fleet SSH stream"));
            remote_stdin.push(stdin_peer);
            remote_stdout.push(stdout_peer);
        }

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            remote_stdout[READY_INDEX]
                .write_all(b"ready")
                .expect("write to the selected fleet descriptor");
            remote_stdout
        });

        asupersync_block_on(async {
            let waiters = streams
                .iter()
                .map(|stream| Box::pin(stream.wait_for_readable()))
                .collect::<Vec<_>>();
            let (result, index, mut still_waiting) = futures::future::select_all(waiters).await;
            result.expect("selected fleet descriptor should become readable");
            assert_eq!(
                index, READY_INDEX,
                "a wake shared by the fleet must not complete an idle descriptor"
            );

            poll_fn(|task_cx| {
                for waiter in &mut still_waiting {
                    assert!(
                        waiter.as_mut().poll(task_cx).is_pending(),
                        "every non-selected descriptor must remain pending"
                    );
                }
                Poll::Ready(())
            })
            .await;
        });

        let remote_stdout = writer.join().expect("fleet writer should join");
        assert_eq!(remote_stdout.len(), FLEET_SIZE);
        assert_eq!(remote_stdin.len(), FLEET_SIZE);
    }

    #[test]
    fn ssh_stream_asupersync_roundtrip_handles_initial_would_block() {
        use asupersync::io::{AsyncReadExt, AsyncWriteExt};

        let (stdin, mut remote_stdin) =
            filedescriptor::socketpair().expect("create stdin socketpair");
        let (mut remote_stdout, stdout) =
            filedescriptor::socketpair().expect("create stdout socketpair");
        let mut stream = SshStream::new(stdin, stdout).expect("construct ssh stream");

        let remote = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            std::thread::sleep(Duration::from_millis(50));
            remote_stdout.write_all(b"ping")?;
            remote_stdout.flush()?;

            let mut buf = [0u8; 4];
            remote_stdin.read_exact(&mut buf)?;
            Ok(buf.to_vec())
        });

        let received = asupersync_block_on(async {
            let mut buf = [0u8; 4];
            AsyncReadExt::read_exact(&mut stream, &mut buf).await?;
            AsyncWriteExt::write_all(&mut stream, b"pong").await?;
            AsyncWriteExt::flush(&mut stream).await?;
            Ok::<Vec<u8>, std::io::Error>(buf.to_vec())
        })
        .expect("client roundtrip should succeed");

        assert_eq!(received, b"ping");
        assert_eq!(
            remote.join().expect("remote thread should join").unwrap(),
            b"pong"
        );
    }
}
