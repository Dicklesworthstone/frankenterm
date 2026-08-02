#![allow(clippy::future_not_send)]
#![allow(clippy::type_repetition_in_bounds)]
use crate::sessionhandler::{PduSender, SessionHandler, SessionOwner};
use anyhow::Context;
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use asupersync::runtime::IoDriverHandle;
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use asupersync::runtime::reactor::{Interest, IoUringReactor};
use async_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use async_ossl::AsyncSslStream;
use codec::{
    DecodedPdu, ListPanesCoherent, ListPanesCoherentOutcome, ListPanesCoherentResponse, Pdu,
    TopologyCapabilities, TopologyEvent, TopologyEventKind, TopologyStreamId,
};
use frankenterm_core::tmux_control_protocol::{TmuxCommand, TmuxResponse, parse_command};
use futures::future::{Either, select};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use futures::task::{ArcWake, waker};
use futures::{FutureExt, pin_mut};
use mux::{
    Mux, MuxNotification, MuxNotificationEnvelope, MuxSessionIncarnation, MuxTopologyStamp,
    TopologyRevision,
};
use parking_lot::Mutex as ParkingMutex;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::{self, ErrorKind};
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;
#[cfg(all(feature = "io-uring", target_os = "linux"))]
use std::task::{Context as TaskContext, Waker};
use wezterm_uds::UnixStream;

pub const DISPATCH_ITEM_QUEUE_CAPACITY: usize = 4096;
const DISPATCH_ITEM_QUEUE_CONTROL_RESERVE: usize = 64;
const DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY: usize =
    DISPATCH_ITEM_QUEUE_CAPACITY + DISPATCH_ITEM_QUEUE_CONTROL_RESERVE;
const TRANSIENT_WRITE_RETRY_LIMIT: usize = 8;
const TMUX_CONTROL_MAX_LINE_BYTES: usize = 16 * 1024;
/// Bound one outbound turn so an already-readable keypress cannot sit behind
/// an arbitrarily long run of render/notification frames.
const OUTBOUND_WRITE_QUANTUM_FRAMES: usize = 32;
const OUTBOUND_WRITE_QUANTUM_BYTES: usize = 64 * 1024;
static DROPPED_NOTIFICATION_COUNT: AtomicU64 = AtomicU64::new(0);

const NOTIFICATION_QUEUE_OVERFLOW: &str =
    "mux notification queue overflowed; topology delivery is no longer lossless";
const NOTIFICATION_QUEUE_CLOSED: &str =
    "mux notification queue closed; topology delivery is no longer possible";
const RESPONSE_QUEUE_FAILURE: &str =
    "mux response queue rejected a frame; request/response delivery is no longer lossless";
const TOPOLOGY_PROTOCOL_FAILURE: &str =
    "mux topology fence observed an impossible stream transition";
const TOPOLOGY_BUFFER_OVERFLOW: &str =
    "mux topology fence exceeded its retained event or byte bound";
const OUTBOUND_BUDGET_OVERFLOW: &str =
    "mux outbound delivery exceeded its retained topology or slot bound";
const TOPOLOGY_REVISION_EXHAUSTED: &str =
    "mux topology revision authority is exhausted";
const TOPOLOGY_FENCE_MAX_EVENTS: usize = 4096;
const TOPOLOGY_FENCE_MAX_RETAINED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundClass {
    Control,
    Bulk,
    Topology,
}

impl OutboundClass {
    const fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Bulk => "bulk",
            Self::Topology => "topology",
        }
    }

    const fn is_bulk(self) -> bool {
        !matches!(self, Self::Control)
    }

    const fn is_topology(self) -> bool {
        matches!(self, Self::Topology)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundBudgetLimit {
    Arithmetic,
    TotalSlots,
    BulkSlots,
    TopologyBytes,
    MixedConnection,
}

impl OutboundBudgetLimit {
    const fn label(self) -> &'static str {
        match self {
            Self::Arithmetic => "arithmetic",
            Self::TotalSlots => "total_slots",
            Self::BulkSlots => "bulk_slots",
            Self::TopologyBytes => "topology_bytes",
            Self::MixedConnection => "mixed_connection",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OutboundBudgetState {
    topology_bytes: usize,
    total_slots: usize,
    bulk_slots: usize,
    peak_topology_bytes: usize,
}

#[derive(Debug, Default)]
struct OutboundBudget {
    state: ParkingMutex<OutboundBudgetState>,
}

impl OutboundBudget {
    fn try_reserve(
        self: &Arc<Self>,
        class: OutboundClass,
        topology_bytes: usize,
    ) -> Result<OutboundReservation, OutboundBudgetLimit> {
        debug_assert_eq!(class.is_topology(), topology_bytes != 0);
        let mut state = self.state.lock();
        let next_total_slots = state
            .total_slots
            .checked_add(1)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_total_slots > DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY {
            return Err(OutboundBudgetLimit::TotalSlots);
        }

        let bulk_slots = usize::from(class.is_bulk());
        let next_bulk_slots = state
            .bulk_slots
            .checked_add(bulk_slots)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_bulk_slots > DISPATCH_ITEM_QUEUE_CAPACITY {
            return Err(OutboundBudgetLimit::BulkSlots);
        }

        let next_topology_bytes = state
            .topology_bytes
            .checked_add(topology_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_topology_bytes > TOPOLOGY_FENCE_MAX_RETAINED_BYTES {
            return Err(OutboundBudgetLimit::TopologyBytes);
        }

        state.total_slots = next_total_slots;
        state.bulk_slots = next_bulk_slots;
        state.topology_bytes = next_topology_bytes;
        state.peak_topology_bytes = state.peak_topology_bytes.max(next_topology_bytes);
        Ok(OutboundReservation {
            budget: Arc::clone(self),
            topology_bytes,
            total_slots: 1,
            bulk_slots,
            is_topology: class.is_topology(),
        })
    }

    fn reweight_topology_batch(
        reservations: &mut [OutboundReservation],
        retained_bytes: usize,
    ) -> Result<(), OutboundBudgetLimit> {
        let Some(first_topology_index) = reservations
            .iter()
            .position(|reservation| reservation.is_topology)
        else {
            return Ok(());
        };
        let budget = Arc::clone(&reservations[first_topology_index].budget);
        if reservations
            .iter()
            .any(|reservation| !Arc::ptr_eq(&budget, &reservation.budget))
        {
            return Err(OutboundBudgetLimit::MixedConnection);
        }
        let current_batch_bytes = reservations
            .iter()
            .filter(|reservation| reservation.is_topology)
            .try_fold(0_usize, |total, reservation| {
                total.checked_add(reservation.topology_bytes)
            })
            .ok_or(OutboundBudgetLimit::Arithmetic)?;

        let mut state = budget.state.lock();
        let other_topology_bytes = state
            .topology_bytes
            .checked_sub(current_batch_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        let next_topology_bytes = other_topology_bytes
            .checked_add(retained_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_topology_bytes > TOPOLOGY_FENCE_MAX_RETAINED_BYTES {
            return Err(OutboundBudgetLimit::TopologyBytes);
        }
        state.topology_bytes = next_topology_bytes;
        state.peak_topology_bytes = state.peak_topology_bytes.max(next_topology_bytes);
        for (index, reservation) in reservations.iter_mut().enumerate() {
            if reservation.is_topology {
                reservation.topology_bytes = if index == first_topology_index {
                    retained_bytes
                } else {
                    0
                };
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn snapshot(&self) -> OutboundBudgetState {
        *self.state.lock()
    }
}

#[derive(Debug)]
struct OutboundReservation {
    budget: Arc<OutboundBudget>,
    topology_bytes: usize,
    total_slots: usize,
    bulk_slots: usize,
    is_topology: bool,
}

impl Drop for OutboundReservation {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock();
        state.topology_bytes = state
            .topology_bytes
            .checked_sub(self.topology_bytes)
            .expect("outbound topology-byte reservation underflow");
        state.total_slots = state
            .total_slots
            .checked_sub(self.total_slots)
            .expect("outbound total-slot reservation underflow");
        state.bulk_slots = state
            .bulk_slots
            .checked_sub(self.bulk_slots)
            .expect("outbound bulk-slot reservation underflow");
    }
}

#[derive(Clone)]
struct DispatchTerminal {
    tripped: Arc<AtomicBool>,
    admission: Arc<ParkingMutex<()>>,
    tx: Sender<&'static str>,
}

struct DispatchAdmission<'a> {
    terminal: &'a DispatchTerminal,
    _guard: parking_lot::MutexGuard<'a, ()>,
}

impl DispatchAdmission<'_> {
    fn trip(&self, reason: &'static str) {
        self.terminal.trip_admitted(reason);
    }
}

impl DispatchTerminal {
    fn channel() -> (Self, Receiver<&'static str>) {
        let (tx, rx) = bounded(1);
        (
            Self {
                tripped: Arc::new(AtomicBool::new(false)),
                admission: Arc::new(ParkingMutex::new(())),
                tx,
            },
            rx,
        )
    }

    fn trip(&self, reason: &'static str) {
        let _admission = self.admission.lock();
        self.trip_admitted(reason);
    }

    fn trip_admitted(&self, reason: &'static str) {
        if self
            .tripped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.tx.try_send(reason);
        }
    }

    fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Acquire)
    }

    fn admit(&self) -> Option<DispatchAdmission<'_>> {
        let guard = self.admission.lock();
        if self.is_tripped() {
            return None;
        }
        Some(DispatchAdmission {
            terminal: self,
            _guard: guard,
        })
    }
}

fn reserve_outbound(
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    class: OutboundClass,
    topology_bytes: usize,
) -> anyhow::Result<OutboundReservation> {
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    let reservation = match budget.try_reserve(class, topology_bytes) {
        Ok(reservation) => reservation,
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            metrics::counter!(
                "mux.dispatch.outbound_budget.rejected",
                "class" => class.label(),
                "limit" => limit.label(),
            )
            .increment(1);
            anyhow::bail!(
                "mux outbound {class:?} reservation exceeded the {} bound",
                limit.label()
            );
        }
    };
    drop(admission);
    Ok(reservation)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchIoBackend {
    IoUring,
    Epoll,
    Kqueue,
    Poll,
}

impl DispatchIoBackend {
    pub const fn readiness_default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Epoll
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            Self::Kqueue
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            Self::Poll
        }
    }

    pub const fn current_default() -> Self {
        #[cfg(all(feature = "io-uring", target_os = "linux"))]
        {
            Self::IoUring
        }
        #[cfg(not(all(feature = "io-uring", target_os = "linux")))]
        {
            Self::readiness_default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchIoPreference {
    Auto,
    IoUring,
    Epoll,
    Kqueue,
    Poll,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchRuntimeConfig {
    preference: DispatchIoPreference,
}

impl DispatchRuntimeConfig {
    #[must_use]
    pub const fn new(preference: DispatchIoPreference) -> Self {
        Self { preference }
    }

    #[must_use]
    pub const fn preference(self) -> DispatchIoPreference {
        self.preference
    }
}

impl Default for DispatchRuntimeConfig {
    fn default() -> Self {
        Self::new(DispatchIoPreference::Auto)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchStreamKind {
    Unix,
    Tls,
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchReadinessHint {
    Ready,
    NotReady,
    Unsupported,
}

/// The side whose readiness completed a combined dispatch wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchReadySide {
    Readable,
    Writable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchIoRuntimeAvailability {
    io_uring_compiled: bool,
    io_uring_runtime_available: bool,
}

impl DispatchIoRuntimeAvailability {
    fn detect() -> Self {
        Self {
            io_uring_compiled: cfg!(all(feature = "io-uring", target_os = "linux")),
            io_uring_runtime_available: io_uring_runtime_available(),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn for_conformance(
        io_uring_compiled: bool,
        io_uring_runtime_available: bool,
    ) -> Self {
        Self {
            io_uring_compiled,
            io_uring_runtime_available,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchReactor {
    requested: DispatchIoPreference,
    backend: DispatchIoBackend,
    fallback_reason: Option<&'static str>,
}

impl DispatchReactor {
    #[must_use]
    pub fn resolve(config: DispatchRuntimeConfig, stream_kind: DispatchStreamKind) -> Self {
        Self::resolve_with_availability(
            config,
            stream_kind,
            DispatchIoRuntimeAvailability::detect(),
        )
    }

    #[doc(hidden)]
    #[must_use]
    pub fn resolve_with_availability(
        config: DispatchRuntimeConfig,
        stream_kind: DispatchStreamKind,
        availability: DispatchIoRuntimeAvailability,
    ) -> Self {
        let fallback_backend = DispatchIoBackend::readiness_default();
        let wants_io_uring = matches!(
            config.preference,
            DispatchIoPreference::Auto | DispatchIoPreference::IoUring
        );

        if wants_io_uring {
            if stream_kind != DispatchStreamKind::Unix {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring mux dispatch currently supports UnixStream only; using readiness backend",
                    ),
                };
            }

            if !availability.io_uring_compiled {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring support was not compiled into this binary; using readiness backend",
                    ),
                };
            }

            if !availability.io_uring_runtime_available {
                return Self {
                    requested: config.preference,
                    backend: fallback_backend,
                    fallback_reason: Some(
                        "io_uring unavailable on this kernel/runtime; using readiness backend",
                    ),
                };
            }

            return Self {
                requested: config.preference,
                backend: DispatchIoBackend::IoUring,
                fallback_reason: None,
            };
        }

        let selected = match config.preference {
            DispatchIoPreference::Auto | DispatchIoPreference::IoUring => unreachable!(),
            DispatchIoPreference::Epoll => {
                #[cfg(target_os = "linux")]
                {
                    DispatchIoBackend::Epoll
                }
                #[cfg(not(target_os = "linux"))]
                {
                    fallback_backend
                }
            }
            DispatchIoPreference::Kqueue => {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                ))]
                {
                    DispatchIoBackend::Kqueue
                }
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                )))]
                {
                    fallback_backend
                }
            }
            DispatchIoPreference::Poll => DispatchIoBackend::Poll,
        };

        let fallback_reason = match (config.preference, selected) {
            (DispatchIoPreference::Epoll, DispatchIoBackend::Epoll)
            | (DispatchIoPreference::Kqueue, DispatchIoBackend::Kqueue)
            | (DispatchIoPreference::Poll, DispatchIoBackend::Poll) => None,
            (DispatchIoPreference::Epoll | DispatchIoPreference::Kqueue, _) => Some(
                "requested dispatch backend is unavailable on this platform; using readiness backend",
            ),
            _ => None,
        };

        Self {
            requested: config.preference,
            backend: selected,
            fallback_reason,
        }
    }

    #[must_use]
    pub const fn backend(self) -> DispatchIoBackend {
        self.backend
    }

    #[must_use]
    pub const fn fallback_reason(self) -> Option<&'static str> {
        self.fallback_reason
    }
}

#[cfg(target_os = "linux")]
fn io_uring_runtime_available() -> bool {
    if !cfg!(feature = "io-uring") {
        return false;
    }

    let Ok(osrelease) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        return false;
    };

    let mut components = osrelease.trim().split(['.', '-']);
    let major = components
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = components
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);

    if (major, minor) < (5, 6) {
        return false;
    }

    match std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled") {
        Ok(flag) => flag.trim() == "0",
        Err(_) => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn io_uring_runtime_available() -> bool {
    false
}

pub trait DispatchStream: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Generic
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>>;
    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>>;

    /// Wait for and classify readable input or writable output.
    ///
    /// Socket wrappers with a single reactor registration slot must override
    /// this, arm a combined interest, and classify the wake without installing
    /// competing registrations. The default remains available for independent
    /// test and generic stream implementations.
    fn wait_for_readable_or_writable(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<DispatchReadySide>> + Send + '_>> {
        let wait_for_read = self.wait_for_readable();
        let wait_for_write = self.wait_for_writable();
        Box::pin(async move {
            pin_mut!(wait_for_read);
            pin_mut!(wait_for_write);
            match select(wait_for_read, wait_for_write).await {
                Either::Left((result, _)) => result.map(|()| DispatchReadySide::Readable),
                Either::Right((result, _)) => result.map(|()| DispatchReadySide::Writable),
            }
        })
    }

    /// Synchronously probe inbound readiness without consuming bytes.
    ///
    /// Production socket wrappers override this so a continuously ready
    /// internal queue cannot win by repeatedly causing a pending-first
    /// readiness future to be created and dropped.
    fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
        Ok(DispatchReadinessHint::Unsupported)
    }

    /// A pending outbound poll may carry a transport-level retry obligation
    /// that forbids application reads until the identical operation is
    /// retried (notably OpenSSL `WANT_READ`/`WANT_WRITE`).
    fn pending_outbound_requires_retry(&self) -> bool {
        false
    }

    fn wait_for_pending_outbound_retry(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        self.wait_for_writable()
    }

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn io_uring_fd(&self) -> Option<RawFd> {
        None
    }
}

impl DispatchStream for UnixStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Unix
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(UnixStream::wait_for_readable(self))
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(UnixStream::wait_for_writable(self))
    }

    fn wait_for_readable_or_writable(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<DispatchReadySide>> + Send + '_>> {
        #[cfg(unix)]
        {
            Box::pin(async move {
                UnixStream::wait_for_readable_or_writable(self).await?;
                UnixStream::try_readable_without_consuming(self).map(|ready| {
                    if ready {
                        DispatchReadySide::Readable
                    } else {
                        DispatchReadySide::Writable
                    }
                })
            })
        }
        #[cfg(not(unix))]
        {
            let wait_for_read = self.wait_for_readable();
            let wait_for_write = self.wait_for_writable();
            Box::pin(async move {
                pin_mut!(wait_for_read);
                pin_mut!(wait_for_write);
                match select(wait_for_read, wait_for_write).await {
                    Either::Left((result, _)) => result.map(|()| DispatchReadySide::Readable),
                    Either::Right((result, _)) => result.map(|()| DispatchReadySide::Writable),
                }
            })
        }
    }

    fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
        #[cfg(unix)]
        {
            UnixStream::try_readable_without_consuming(self).map(|ready| {
                if ready {
                    DispatchReadinessHint::Ready
                } else {
                    DispatchReadinessHint::NotReady
                }
            })
        }
        #[cfg(not(unix))]
        {
            Ok(DispatchReadinessHint::Unsupported)
        }
    }

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn io_uring_fd(&self) -> Option<RawFd> {
        Some(self.as_raw_fd())
    }
}

impl DispatchStream for AsyncSslStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Tls
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(AsyncSslStream::wait_for_readable(self))
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(AsyncSslStream::wait_for_writable(self))
    }

    fn wait_for_readable_or_writable(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<DispatchReadySide>> + Send + '_>> {
        Box::pin(async move {
            AsyncSslStream::wait_for_readable_or_writable(self).await?;
            AsyncSslStream::try_readable_without_consuming(self).map(|ready| {
                if ready {
                    DispatchReadySide::Readable
                } else {
                    DispatchReadySide::Writable
                }
            })
        })
    }

    fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
        AsyncSslStream::try_readable_without_consuming(self).map(|ready| {
            if ready {
                DispatchReadinessHint::Ready
            } else {
                DispatchReadinessHint::NotReady
            }
        })
    }

    fn pending_outbound_requires_retry(&self) -> bool {
        AsyncSslStream::pending_outbound_requires_retry(self)
    }

    fn wait_for_pending_outbound_retry(
        &self,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(AsyncSslStream::wait_for_pending_outbound_retry(self))
    }
}

#[derive(Debug)]
struct ReservedNotification {
    notification: MuxNotification,
    reservation: OutboundReservation,
}

#[derive(Debug)]
struct ReservedDecodedPdu {
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
}

#[derive(Debug)]
struct EncodedOutboundFrame {
    bytes: Vec<u8>,
    reservation: OutboundReservation,
}

#[derive(Debug)]
enum WritePayload {
    Typed(ReservedDecodedPdu),
    Encoded(EncodedOutboundFrame),
}

#[derive(Debug)]
enum Item {
    Notif(ReservedNotification),
    WritePdu(WritePayload),
    Readable,
}

struct MuxSubscriptionGuard {
    mux: Arc<Mux>,
    sub_id: usize,
}

impl MuxSubscriptionGuard {
    fn new(mux: Arc<Mux>, sub_id: usize) -> Self {
        Self { mux, sub_id }
    }

    fn bind_topology(
        self,
        topology: &TopologyStreamCoordinator,
        session_incarnation: MuxSessionIncarnation,
        baseline_revision: TopologyRevision,
    ) -> anyhow::Result<Self> {
        topology.bind_subscription(session_incarnation, baseline_revision)?;
        Ok(self)
    }
}

impl Drop for MuxSubscriptionGuard {
    fn drop(&mut self) {
        let _ = self.mux.unsubscribe(self.sub_id);
    }
}

fn pdu_item(
    pdu: Pdu,
    serial: u64,
    reservation: OutboundReservation,
) -> Item {
    Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu {
        decoded: Box::new(DecodedPdu { pdu, serial }),
        reservation,
    }))
}

#[cfg(test)]
fn test_write_payload(decoded: Box<DecodedPdu>) -> WritePayload {
    let budget = Arc::new(OutboundBudget::default());
    let reservation = budget
        .try_reserve(OutboundClass::Control, 0)
        .expect("an empty test outbound budget should admit one control item");
    WritePayload::Typed(ReservedDecodedPdu {
        decoded,
        reservation,
    })
}

#[cfg(test)]
fn test_write_item(decoded: Box<DecodedPdu>) -> Item {
    Item::WritePdu(test_write_payload(decoded))
}

#[cfg(test)]
fn test_notification_item(notification: MuxNotification) -> Item {
    let budget = Arc::new(OutboundBudget::default());
    let reservation = budget
        .try_reserve(OutboundClass::Bulk, 0)
        .expect("an empty test outbound budget should admit one bulk item");
    Item::Notif(ReservedNotification {
        notification,
        reservation,
    })
}

#[cfg(test)]
fn queue_pdu(item_tx: &Sender<Item>, pdu: Pdu, serial: u64) -> anyhow::Result<()> {
    let budget = Arc::new(OutboundBudget::default());
    let reservation = budget
        .try_reserve(OutboundClass::Control, 0)
        .expect("an empty test outbound budget should admit one control item");
    match item_tx.try_send(pdu_item(pdu, serial, reservation)) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(anyhow::anyhow!(
            "mux dispatch item queue is full (capacity {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
        )),
        Err(TrySendError::Closed(_)) => Err(anyhow::anyhow!("mux dispatch item queue is closed")),
    }
}

fn queue_response_pdu(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    pdu: Pdu,
    serial: u64,
) -> anyhow::Result<()> {
    // Avoid even the outbound Box allocation on an already-dead connection;
    // `admit` below remains the authoritative race-closing check.
    if terminal.is_tripped() {
        anyhow::bail!("mux dispatch connection is already terminal");
    }
    // Box the response before entering the connection's short admission
    // section. Large coherent snapshots must not extend this critical section
    // with allocator work.
    let reservation = reserve_outbound(terminal, budget, OutboundClass::Control, 0)?;
    let item = pdu_item(pdu, serial, reservation);
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(error) => {
            admission.trip(RESPONSE_QUEUE_FAILURE);
            drop(admission);
            metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
            match error {
                TrySendError::Full(_item) => Err(anyhow::anyhow!(
                    "mux dispatch item queue is full (capacity \
                     {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
                )),
                TrySendError::Closed(_item) => {
                    Err(anyhow::anyhow!("mux dispatch item queue is closed"))
                }
            }
        }
    }
}

fn queue_reserved_pdu(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
) -> anyhow::Result<()> {
    let item = Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu {
        decoded,
        reservation,
    }));
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(item)) => {
            admission.trip(NOTIFICATION_QUEUE_OVERFLOW);
            drop(admission);
            drop(item);
            metrics::counter!("mux.dispatch.notification_queue.full").increment(1);
            Err(anyhow::anyhow!(
                "mux dispatch topology queue is full (bulk capacity \
                 {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
            ))
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(NOTIFICATION_QUEUE_CLOSED);
            drop(admission);
            drop(item);
            Err(anyhow::anyhow!("mux dispatch topology queue is closed"))
        }
    }
}

fn queue_reserved_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    notification: MuxNotification,
    reservation: OutboundReservation,
) -> bool {
    let item = Item::Notif(ReservedNotification {
        notification,
        reservation,
    });
    let Some(admission) = terminal.admit() else {
        return false;
    };
    match item_tx.try_send(item) {
        Ok(()) => true,
        Err(TrySendError::Full(item)) => {
            admission.trip(NOTIFICATION_QUEUE_OVERFLOW);
            drop(admission);
            drop(item);
            let dropped = DROPPED_NOTIFICATION_COUNT
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            metrics::counter!("mux.dispatch.notification_queue.full").increment(1);
            if dropped.is_power_of_two() {
                log::warn!(
                    "mux dispatch notification queue is full (bulk capacity \
                     {DISPATCH_ITEM_QUEUE_CAPACITY}); terminating the affected connection after \
                     {dropped} overflow(s) since process start"
                );
            }
            false
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(NOTIFICATION_QUEUE_CLOSED);
            drop(admission);
            drop(item);
            false
        }
    }
}

fn queue_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    notification: MuxNotification,
) -> bool {
    let Ok(reservation) = reserve_outbound(terminal, budget, OutboundClass::Bulk, 0) else {
        return false;
    };
    queue_reserved_notification(item_tx, terminal, notification, reservation)
}

#[derive(Debug)]
struct RetainedTopologyEvent {
    notification: MuxNotification,
    revision: TopologyRevision,
    retained_bytes: usize,
    reservation: OutboundReservation,
}

// This covers the retained value and the separately stored BTreeMap key.  The
// allocator's node metadata is implementation-defined rather than byte-true;
// its residual overhead is independently bounded by TOPOLOGY_FENCE_MAX_EVENTS.
const RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES: usize =
    std::mem::size_of::<RetainedTopologyEvent>() + std::mem::size_of::<TopologyRevision>();

#[derive(Debug, Default)]
struct TopologyEventBuffer {
    events: BTreeMap<TopologyRevision, RetainedTopologyEvent>,
    retained_bytes: usize,
}

impl TopologyEventBuffer {
    fn insert(&mut self, event: RetainedTopologyEvent) -> anyhow::Result<()> {
        if self.events.contains_key(&event.revision) {
            anyhow::bail!(
                "duplicate mux topology revision {}",
                event.revision.get()
            );
        }
        let next_len = self
            .events
            .len()
            .checked_add(1)
            .context("counting retained mux topology events")?;
        let next_bytes = self
            .retained_bytes
            .checked_add(event.retained_bytes)
            .context("counting retained mux topology bytes")?;
        if next_len > TOPOLOGY_FENCE_MAX_EVENTS
            || next_bytes > TOPOLOGY_FENCE_MAX_RETAINED_BYTES
        {
            anyhow::bail!(
                "mux topology fence buffer would retain {next_len} events and {next_bytes} bytes"
            );
        }
        self.retained_bytes = next_bytes;
        self.events.insert(event.revision, event);
        metrics::histogram!("mux.dispatch.topology_fence.retained_events")
            .record(next_len as f64);
        metrics::histogram!("mux.dispatch.topology_fence.retained_bytes")
            .record(next_bytes as f64);
        Ok(())
    }

    fn remove(
        &mut self,
        revision: TopologyRevision,
    ) -> anyhow::Result<Option<RetainedTopologyEvent>> {
        let Some(event) = self.events.remove(&revision) else {
            return Ok(None);
        };
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(event.retained_bytes)
            .context("decrementing retained mux topology bytes")?;
        Ok(Some(event))
    }

    fn take_all(&mut self) -> impl Iterator<Item = RetainedTopologyEvent> {
        self.retained_bytes = 0;
        std::mem::take(&mut self.events).into_values()
    }
}

#[derive(Clone, Copy, Debug)]
struct TopologySubscriptionAuthority {
    session_incarnation: MuxSessionIncarnation,
    baseline_revision: TopologyRevision,
}

#[derive(Debug)]
enum TopologyFencePrior {
    Legacy,
    Established {
        snapshot_revision: TopologyRevision,
        next_revision: Option<TopologyRevision>,
    },
}

#[derive(Debug)]
struct TopologyFenceInFlight {
    serial: u64,
    negotiated: TopologyCapabilities,
    prior: TopologyFencePrior,
    buffer: TopologyEventBuffer,
}

#[derive(Debug)]
struct EstablishedTopologyStream {
    snapshot_revision: TopologyRevision,
    next_revision: Option<TopologyRevision>,
    buffer: TopologyEventBuffer,
}

#[derive(Debug)]
enum TopologyStreamPhase {
    Legacy,
    Fencing(TopologyFenceInFlight),
    Established(EstablishedTopologyStream),
    Exhausted,
}

#[derive(Debug)]
struct TopologyStreamState {
    subscription: Option<TopologySubscriptionAuthority>,
    prebind: TopologyEventBuffer,
    phase: TopologyStreamPhase,
}

impl Default for TopologyStreamState {
    fn default() -> Self {
        Self {
            subscription: None,
            prebind: TopologyEventBuffer::default(),
            phase: TopologyStreamPhase::Legacy,
        }
    }
}

struct TopologyStreamCoordinator {
    item_tx: Sender<Item>,
    terminal: DispatchTerminal,
    outbound_budget: Arc<OutboundBudget>,
    stream_id: TopologyStreamId,
    state: ParkingMutex<TopologyStreamState>,
}

impl TopologyStreamCoordinator {
    fn new(
        item_tx: Sender<Item>,
        terminal: DispatchTerminal,
        stream_id: TopologyStreamId,
    ) -> Self {
        Self {
            item_tx,
            terminal,
            outbound_budget: Arc::new(OutboundBudget::default()),
            stream_id,
            state: ParkingMutex::new(TopologyStreamState::default()),
        }
    }

    fn discard_retained_state(&self) {
        let mut state = self.state.lock();
        state.prebind = TopologyEventBuffer::default();
        state.phase = TopologyStreamPhase::Exhausted;
    }

    fn with_live_result<T>(
        &self,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if self.terminal.is_tripped() {
            self.discard_retained_state();
            anyhow::bail!("mux dispatch connection is already terminal");
        }
        let result = operation();
        if self.terminal.admit().is_none() {
            self.discard_retained_state();
            if result.is_ok() {
                anyhow::bail!("mux dispatch connection became terminal during admission");
            }
        }
        result
    }

    fn bind_subscription(
        &self,
        session_incarnation: MuxSessionIncarnation,
        baseline_revision: TopologyRevision,
    ) -> anyhow::Result<()> {
        self.with_live_result(|| {
            self.bind_subscription_admitted(session_incarnation, baseline_revision)
        })
    }

    fn bind_subscription_admitted(
        &self,
        session_incarnation: MuxSessionIncarnation,
        baseline_revision: TopologyRevision,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if state.subscription.is_some() {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("mux topology stream subscription was bound more than once");
        }
        state.subscription = Some(TopologySubscriptionAuthority {
            session_incarnation,
            baseline_revision,
        });

        for event in state.prebind.take_all() {
            if event.revision <= baseline_revision {
                continue;
            }
            if !queue_reserved_notification(
                &self.item_tx,
                &self.terminal,
                event.notification,
                event.reservation,
            ) {
                anyhow::bail!("failed to enqueue a pre-bind mux topology notification");
            }
        }
        Ok(())
    }

    fn begin_fence(&self, serial: u64, request: &ListPanesCoherent) -> anyhow::Result<()> {
        self.with_live_result(|| self.begin_fence_admitted(serial, request))
    }

    fn begin_fence_admitted(
        &self,
        serial: u64,
        request: &ListPanesCoherent,
    ) -> anyhow::Result<()> {
        let negotiated = request
            .supported
            .intersection(TopologyCapabilities::SERVER_SUPPORTED);
        if !negotiated.contains(TopologyCapabilities::FENCED_SNAPSHOT_V1)
            || !negotiated.contains(request.required)
        {
            metrics::counter!(
                "mux.dispatch.topology_fence.negotiation.total",
                "outcome" => "unsupported"
            )
            .increment(1);
            return Ok(());
        }
        metrics::counter!(
            "mux.dispatch.topology_fence.negotiation.total",
            "outcome" => "admitted"
        )
        .increment(1);

        let mut state = self.state.lock();
        if state.subscription.is_none() {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("mux topology fence began before its subscription was bound");
        }
        let phase = std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
        state.phase = match phase {
            TopologyStreamPhase::Legacy => {
                TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                    serial,
                    negotiated,
                    prior: TopologyFencePrior::Legacy,
                    buffer: TopologyEventBuffer::default(),
                })
            }
            TopologyStreamPhase::Established(established) => {
                TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                    serial,
                    negotiated,
                    prior: TopologyFencePrior::Established {
                        snapshot_revision: established.snapshot_revision,
                        next_revision: established.next_revision,
                    },
                    buffer: established.buffer,
                })
            }
            TopologyStreamPhase::Fencing(in_flight) => {
                state.phase = TopologyStreamPhase::Fencing(in_flight);
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!("overlapping coherent mux topology snapshot requests")
            }
            TopologyStreamPhase::Exhausted => {
                state.phase = TopologyStreamPhase::Exhausted;
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
                anyhow::bail!("mux topology stream is exhausted")
            }
        };
        Ok(())
    }

    fn queue_response(&self, decoded: DecodedPdu) -> anyhow::Result<()> {
        self.with_live_result(|| self.queue_response_admitted(decoded))
    }

    fn queue_response_admitted(&self, decoded: DecodedPdu) -> anyhow::Result<()> {
        let DecodedPdu { pdu, serial } = decoded;
        let mut state = self.state.lock();
        match pdu {
            Pdu::ListPanesCoherentResponse(response) => self
                .complete_fence_response(&mut state, serial, response)
                .inspect_err(|_| self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE)),
            other => {
                let phase =
                    std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
                let result = (|| {
                    match phase {
                        TopologyStreamPhase::Fencing(in_flight)
                            if in_flight.serial == serial =>
                        {
                            queue_response_pdu(
                                &self.item_tx,
                                &self.terminal,
                                &self.outbound_budget,
                                other,
                                serial,
                            )?;
                            state.phase = self.restore_prior(in_flight)?;
                        }
                        phase => {
                            state.phase = phase;
                            queue_response_pdu(
                                &self.item_tx,
                                &self.terminal,
                                &self.outbound_budget,
                                other,
                                serial,
                            )?;
                        }
                    }
                    Ok(())
                })();
                result.inspect_err(|_| self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE))
            }
        }
    }

    fn complete_fence_response(
        &self,
        state: &mut TopologyStreamState,
        serial: u64,
        response: ListPanesCoherentResponse,
    ) -> anyhow::Result<()> {
        if response.stream_id != self.stream_id {
            state.phase = TopologyStreamPhase::Exhausted;
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("coherent mux snapshot response carried the wrong topology stream id");
        }

        let phase = std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
        let TopologyStreamPhase::Fencing(mut in_flight) = phase else {
            state.phase = phase;
            if matches!(
                &response.outcome,
                ListPanesCoherentOutcome::Unsupported { .. }
            ) {
                return queue_response_pdu(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                );
            }
            state.phase = TopologyStreamPhase::Exhausted;
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("coherent mux snapshot response arrived without an active fence");
        };
        if in_flight.serial != serial || in_flight.negotiated != response.negotiated {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("coherent mux snapshot response did not match its request fence");
        }

        // Classify the result by reference and retain only the two copy-sized
        // authority fields needed after `response` moves into the outbound
        // queue. Cloning `response.outcome` here would deep-clone the complete
        // pane snapshot on every successful fence.
        enum FenceOutcomeAuthority {
            Snapshot {
                session_incarnation: MuxSessionIncarnation,
                snapshot_revision: TopologyRevision,
            },
            Contended,
            RevisionExhausted,
            Unsupported,
        }

        let outcome = match &response.outcome {
            ListPanesCoherentOutcome::Snapshot(snapshot) => {
                FenceOutcomeAuthority::Snapshot {
                    session_incarnation: snapshot.session_incarnation,
                    snapshot_revision: snapshot.snapshot_revision,
                }
            }
            ListPanesCoherentOutcome::Contended { .. } => FenceOutcomeAuthority::Contended,
            ListPanesCoherentOutcome::RevisionExhausted => {
                FenceOutcomeAuthority::RevisionExhausted
            }
            ListPanesCoherentOutcome::Unsupported { .. } => FenceOutcomeAuthority::Unsupported,
        };

        match outcome {
            FenceOutcomeAuthority::Snapshot {
                session_incarnation,
                snapshot_revision,
            } => {
                let subscription = state
                    .subscription
                    .context("coherent mux snapshot completed before subscription binding")?;
                let wrong_session_incarnation =
                    session_incarnation != subscription.session_incarnation;
                let snapshot_predates_subscription =
                    snapshot_revision < subscription.baseline_revision;
                let revision_namespace_exhausted = snapshot_revision.get() == u64::MAX;
                if wrong_session_incarnation
                    || snapshot_predates_subscription
                    || revision_namespace_exhausted
                {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!(
                        "coherent mux snapshot authority did not match the connection subscription"
                    );
                }
                queue_response_pdu(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                )?;

                let mut established = EstablishedTopologyStream {
                    snapshot_revision,
                    next_revision: snapshot_revision
                        .get()
                        .checked_add(1)
                        .map(TopologyRevision::new),
                    buffer: TopologyEventBuffer::default(),
                };
                for event in in_flight.buffer.take_all() {
                    if event.revision > snapshot_revision {
                        established.buffer.insert(event).inspect_err(|_| {
                            self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        })?;
                    } else {
                        metrics::counter!(
                            "mux.dispatch.topology_fence.events.total",
                            "outcome" => "snapshot_subsumed"
                        )
                        .increment(1);
                    }
                }
                self.drain_established(&mut established)?;
                state.phase = TopologyStreamPhase::Established(established);
            }
            FenceOutcomeAuthority::Contended => {
                queue_response_pdu(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                )?;
                state.phase = self.restore_prior(in_flight)?;
            }
            FenceOutcomeAuthority::RevisionExhausted => {
                queue_response_pdu(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                )?;
                state.phase = TopologyStreamPhase::Exhausted;
            }
            FenceOutcomeAuthority::Unsupported => {
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!(
                    "coherent mux snapshot became unsupported after its fence was admitted"
                );
            }
        }
        Ok(())
    }

    fn restore_prior(
        &self,
        mut in_flight: TopologyFenceInFlight,
    ) -> anyhow::Result<TopologyStreamPhase> {
        match in_flight.prior {
            TopologyFencePrior::Legacy => {
                for event in in_flight.buffer.take_all() {
                    if !queue_reserved_notification(
                        &self.item_tx,
                        &self.terminal,
                        event.notification,
                        event.reservation,
                    ) {
                        anyhow::bail!(
                            "failed to restore a buffered legacy mux topology notification"
                        );
                    }
                }
                Ok(TopologyStreamPhase::Legacy)
            }
            TopologyFencePrior::Established {
                snapshot_revision,
                next_revision,
            } => {
                let mut established = EstablishedTopologyStream {
                    snapshot_revision,
                    next_revision,
                    buffer: in_flight.buffer,
                };
                self.drain_established(&mut established)?;
                Ok(TopologyStreamPhase::Established(established))
            }
        }
    }

    fn drain_established(
        &self,
        established: &mut EstablishedTopologyStream,
    ) -> anyhow::Result<()> {
        while let Some(next_revision) = established.next_revision {
            let Some(event) = established.buffer.remove(next_revision)? else {
                break;
            };
            self.queue_stamped_event(event)?;
            metrics::counter!(
                "mux.dispatch.topology_fence.events.total",
                "outcome" => "replayed"
            )
            .increment(1);
            established.next_revision = next_revision
                .get()
                .checked_add(1)
                .map(TopologyRevision::new);
        }
        Ok(())
    }

    fn queue_stamped_event(&self, event: RetainedTopologyEvent) -> anyhow::Result<()> {
        let RetainedTopologyEvent {
            notification,
            revision,
            reservation,
            ..
        } = event;
        queue_reserved_pdu(
            &self.item_tx,
            &self.terminal,
            Box::new(DecodedPdu {
                pdu: Pdu::TopologyEvent(TopologyEvent {
                    stream_id: self.stream_id,
                    revision,
                    event: into_topology_event_kind(notification)?,
                }),
                serial: 0,
            }),
            reservation,
        )
    }

    fn on_notification(&self, _mux: &Mux, envelope: MuxNotificationEnvelope) -> bool {
        if self.terminal.is_tripped() {
            self.discard_retained_state();
            return false;
        }
        let revision = match envelope.topology {
            MuxTopologyStamp::NonTopology => {
                if envelope.notification.is_topology() {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    self.discard_retained_state();
                    return false;
                }
                let accepted =
                    queue_notification(
                        &self.item_tx,
                        &self.terminal,
                        &self.outbound_budget,
                        envelope.notification,
                    );
                if !accepted && self.terminal.is_tripped() {
                    self.discard_retained_state();
                }
                return accepted;
            }
            MuxTopologyStamp::Revision(revision) => {
                if !envelope.notification.is_topology() {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    self.discard_retained_state();
                    return false;
                }
                revision
            }
            MuxTopologyStamp::Exhausted => {
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
                self.discard_retained_state();
                return false;
            }
        };
        let accepted = self.on_topology_notification(envelope.notification, revision);
        if self.terminal.admit().is_none() {
            self.discard_retained_state();
            false
        } else {
            accepted
        }
    }

    fn on_topology_notification(
        &self,
        notification: MuxNotification,
        revision: TopologyRevision,
    ) -> bool {
        {
            let state = self.state.lock();
            if state.subscription.is_some()
                && matches!(&state.phase, TopologyStreamPhase::Legacy)
            {
                // Keep the coordinator lock through enqueue. Otherwise this
                // callback can observe Legacy, lose the lock to begin_fence,
                // and append an unstamped notification after the fence has
                // begun. Whichever side acquires this lock first now defines
                // whether the event is a predecessor legacy frame or a
                // retained stamped successor.
                let event = match retained_topology_event(
                    notification,
                    revision,
                    &self.terminal,
                    &self.outbound_budget,
                ) {
                    Ok(event) => event,
                    Err(err) => {
                        log::error!("failed to retain legacy mux topology event: {err:#}");
                        self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        return false;
                    }
                };
                return queue_reserved_notification(
                    &self.item_tx,
                    &self.terminal,
                    event.notification,
                    event.reservation,
                );
            }
        }

        let event = match retained_topology_event(
            notification,
            revision,
            &self.terminal,
            &self.outbound_budget,
        ) {
            Ok(event) => event,
            Err(err) => {
                log::error!("failed to retain mux topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                return false;
            }
        };

        let mut state = self.state.lock();
        if state.subscription.is_none() {
            if let Err(err) = state.prebind.insert(event) {
                log::error!("failed to retain pre-bind mux topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                return false;
            }
            return true;
        }

        let phase = &mut state.phase;
        match phase {
            TopologyStreamPhase::Legacy => {
                queue_reserved_notification(
                    &self.item_tx,
                    &self.terminal,
                    event.notification,
                    event.reservation,
                )
            }
            TopologyStreamPhase::Fencing(in_flight) => {
                if let Err(err) = in_flight.buffer.insert(event) {
                    log::error!("failed to retain in-flight mux topology event: {err:#}");
                    self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                    false
                } else {
                    true
                }
            }
            TopologyStreamPhase::Established(established) => {
                if event.revision <= established.snapshot_revision {
                    return true;
                }
                let Some(next_revision) = established.next_revision else {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    return false;
                };
                if event.revision < next_revision {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    return false;
                }
                if event.revision == next_revision {
                    if let Err(err) = self.queue_stamped_event(event) {
                        log::error!("failed to enqueue contiguous mux topology event: {err:#}");
                        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                        return false;
                    }
                    established.next_revision = next_revision
                        .get()
                        .checked_add(1)
                        .map(TopologyRevision::new);
                    if let Err(err) = self.drain_established(established) {
                        log::error!("failed to drain reordered mux topology events: {err:#}");
                        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                        return false;
                    }
                } else if let Err(err) = established.buffer.insert(event) {
                    log::error!("failed to retain gapped mux topology event: {err:#}");
                    self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                    return false;
                } else {
                    metrics::counter!(
                        "mux.dispatch.topology_fence.events.total",
                        "outcome" => "gap_buffered"
                    )
                    .increment(1);
                }
                true
            }
            TopologyStreamPhase::Exhausted => {
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
                false
            }
        }
    }
}

fn retained_topology_event(
    notification: MuxNotification,
    revision: TopologyRevision,
    terminal: &DispatchTerminal,
    outbound_budget: &Arc<OutboundBudget>,
) -> anyhow::Result<RetainedTopologyEvent> {
    let dynamic_bytes = match &notification {
        MuxNotification::WindowWorkspaceChanged { workspace, .. } => workspace.capacity(),
        MuxNotification::TabTitleChanged { title, .. }
        | MuxNotification::WindowTitleChanged { title, .. } => title.capacity(),
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => old_workspace
            .capacity()
            .checked_add(new_workspace.capacity())
            .context("counting retained workspace rename bytes")?,
        MuxNotification::PaneAdded(_)
        | MuxNotification::PaneRemoved(_)
        | MuxNotification::WindowCreated(_)
        | MuxNotification::WindowRemoved(_)
        | MuxNotification::WindowInvalidated(_)
        | MuxNotification::Empty
        | MuxNotification::TabAddedToWindow { .. }
        | MuxNotification::PaneFocused(_)
        | MuxNotification::TabResized(_) => 0,
        MuxNotification::PaneOutput(_)
        | MuxNotification::SynchronizedOutput { .. }
        | MuxNotification::ActiveWorkspaceChanged(_)
        | MuxNotification::Alert { .. }
        | MuxNotification::AssignClipboard { .. }
        | MuxNotification::SaveToDownloads { .. } => {
            anyhow::bail!("non-topology mux notification carried a topology revision")
        }
    };
    let retained_bytes = RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES
        .checked_add(dynamic_bytes)
        .context("counting retained mux topology event bytes")?;
    let reservation = reserve_outbound(
        terminal,
        outbound_budget,
        OutboundClass::Topology,
        retained_bytes,
    )?;
    Ok(RetainedTopologyEvent {
        notification,
        revision,
        retained_bytes,
        reservation,
    })
}

fn into_topology_event_kind(notification: MuxNotification) -> anyhow::Result<TopologyEventKind> {
    let event = match notification {
        MuxNotification::PaneAdded(pane_id) => TopologyEventKind::PaneAdded { pane_id },
        MuxNotification::PaneRemoved(pane_id) => TopologyEventKind::PaneRemoved { pane_id },
        MuxNotification::WindowCreated(window_id) => {
            TopologyEventKind::WindowCreated { window_id }
        }
        MuxNotification::WindowRemoved(window_id) => {
            TopologyEventKind::WindowRemoved { window_id }
        }
        MuxNotification::WindowInvalidated(window_id) => {
            TopologyEventKind::WindowInvalidated { window_id }
        }
        MuxNotification::WindowWorkspaceChanged {
            window_id,
            workspace,
        } => TopologyEventKind::WindowWorkspaceChanged {
            window_id,
            workspace: Some(workspace),
        },
        MuxNotification::Empty => TopologyEventKind::Empty,
        MuxNotification::TabAddedToWindow { tab_id, window_id } => {
            TopologyEventKind::TabAddedToWindow { tab_id, window_id }
        }
        MuxNotification::PaneFocused(pane_id) => TopologyEventKind::PaneFocused { pane_id },
        MuxNotification::TabResized(tab_id) => TopologyEventKind::TabResized { tab_id },
        MuxNotification::TabTitleChanged { tab_id, title } => {
            TopologyEventKind::TabTitleChanged { tab_id, title }
        }
        MuxNotification::WindowTitleChanged { window_id, title } => {
            TopologyEventKind::WindowTitleChanged { window_id, title }
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => TopologyEventKind::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        },
        MuxNotification::PaneOutput(_)
        | MuxNotification::SynchronizedOutput { .. }
        | MuxNotification::ActiveWorkspaceChanged(_)
        | MuxNotification::Alert { .. }
        | MuxNotification::AssignClipboard { .. }
        | MuxNotification::SaveToDownloads { .. } => {
            anyhow::bail!("non-topology mux notification carried a topology revision")
        }
    };
    Ok(event)
}

fn prepare_unilateral_pdu(
    pdu: Pdu,
    reservation: OutboundReservation,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    terminal: &DispatchTerminal,
) -> anyhow::Result<PendingOutboundBatch> {
    prepare_pending_outbound_batch(
        WritePayload::Typed(ReservedDecodedPdu {
            decoded: Box::new(DecodedPdu { pdu, serial: 0 }),
            reservation,
        }),
        item_rx,
        deferred_item,
        codec::CompressionMode::Auto,
        terminal,
    )
}

fn is_clean_disconnect(err: &anyhow::Error) -> bool {
    err.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io_err| {
            matches!(
                io_err.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionReset
                    | ErrorKind::NotConnected
            )
        })
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Clone, Copy)]
struct DispatchRawFdSource {
    raw_fd: RawFd,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl std::os::fd::AsRawFd for DispatchRawFdSource {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Default)]
struct DispatchIoUringWaiter {
    outcome: ParkingMutex<Option<std::io::Result<()>>>,
    task_waker: ParkingMutex<Option<Waker>>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringWaiter {
    fn set_task_waker(&self, waker: &Waker) {
        let mut slot = self.task_waker.lock();
        if slot
            .as_ref()
            .is_none_or(|existing| !existing.will_wake(waker))
        {
            *slot = Some(waker.clone());
        }
    }

    fn finish(&self, result: std::io::Result<()>) {
        {
            let mut outcome = self.outcome.lock();
            if outcome.is_none() {
                *outcome = Some(result);
            }
        }

        if let Some(waker) = self.task_waker.lock().as_ref().cloned() {
            waker.wake();
        }
    }

    fn take_outcome(&self) -> Option<std::io::Result<()>> {
        self.outcome.lock().take()
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl ArcWake for DispatchIoUringWaiter {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.finish(Ok(()));
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
#[derive(Default)]
struct DispatchIoUringRuntimeState {
    waiters: ParkingMutex<Vec<std::sync::Weak<DispatchIoUringWaiter>>>,
    poll_error: ParkingMutex<Option<(std::io::ErrorKind, String)>>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringRuntimeState {
    fn track_waiter(&self, waiter: &Arc<DispatchIoUringWaiter>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|existing| existing.strong_count() > 0);
        waiters.push(Arc::downgrade(waiter));
    }

    fn poll_error(&self) -> Option<std::io::Error> {
        self.poll_error
            .lock()
            .as_ref()
            .map(|(kind, message)| std::io::Error::new(*kind, message.clone()))
    }

    fn fail_waiters(&self, kind: std::io::ErrorKind, message: String) {
        *self.poll_error.lock() = Some((kind, message.clone()));
        let mut waiters = self.waiters.lock();
        waiters.retain(|waiter| {
            if let Some(waiter) = waiter.upgrade() {
                waiter.finish(Err(std::io::Error::new(kind, message.clone())));
                true
            } else {
                false
            }
        });
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
struct DispatchIoUringRuntime {
    driver: IoDriverHandle,
    state: Arc<DispatchIoUringRuntimeState>,
    shutdown: Arc<AtomicBool>,
    poller: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(all(feature = "io-uring", target_os = "linux")))]
struct DispatchIoUringRuntime;

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl DispatchIoUringRuntime {
    fn maybe_new(reactor: DispatchReactor, raw_fd: Option<RawFd>) -> Option<Self> {
        if reactor.backend() != DispatchIoBackend::IoUring || raw_fd.is_none() {
            return None;
        }

        match Self::new() {
            Ok(runtime) => Some(runtime),
            Err(err) => {
                log::warn!(
                    "io_uring mux dispatch backend selected but runtime init failed; falling back to readiness path: {err}"
                );
                None
            }
        }
    }

    fn new() -> std::io::Result<Self> {
        let reactor = Arc::new(IoUringReactor::new()?);
        let driver = IoDriverHandle::new(reactor);
        let state = Arc::new(DispatchIoUringRuntimeState::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let poll_driver = driver.clone();
        let poll_state = Arc::clone(&state);
        let poll_shutdown = Arc::clone(&shutdown);
        let poller = std::thread::Builder::new()
            .name("ft-mux-io-uring".to_string())
            .spawn(move || {
                while !poll_shutdown.load(Ordering::Acquire) {
                    match poll_driver.turn_with(None, |_, _| {}) {
                        Ok(_) => {}
                        Err(err) => {
                            poll_state.fail_waiters(
                                err.kind(),
                                format!("io_uring mux dispatch poll failed: {err}"),
                            );
                            break;
                        }
                    }
                }
            })
            .map_err(std::io::Error::other)?;

        Ok(Self {
            driver,
            state,
            shutdown,
            poller: Some(poller),
        })
    }

    fn wait_for_fd(&self, raw_fd: RawFd, interest: Interest) -> DispatchIoUringWaitFuture<'_> {
        DispatchIoUringWaitFuture {
            runtime: self,
            raw_fd,
            interest,
            waiter: Arc::new(DispatchIoUringWaiter::default()),
            registration: None,
        }
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl Drop for DispatchIoUringRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.driver.wake();
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
struct DispatchIoUringWaitFuture<'a> {
    runtime: &'a DispatchIoUringRuntime,
    raw_fd: RawFd,
    interest: Interest,
    waiter: Arc<DispatchIoUringWaiter>,
    registration: Option<asupersync::runtime::IoRegistration>,
}

#[cfg(all(feature = "io-uring", target_os = "linux"))]
impl Future for DispatchIoUringWaitFuture<'_> {
    type Output = std::io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.waiter.take_outcome() {
            self.registration.take();
            return Poll::Ready(result);
        }

        if let Some(err) = self.runtime.state.poll_error() {
            return Poll::Ready(Err(err));
        }

        self.waiter.set_task_waker(cx.waker());

        if self.registration.is_none() {
            self.runtime.state.track_waiter(&self.waiter);
            let source = DispatchRawFdSource {
                raw_fd: self.raw_fd,
            };
            self.registration = Some(self.runtime.driver.register(
                &source,
                self.interest,
                waker(Arc::clone(&self.waiter)),
            )?);
        }

        if let Some(result) = self.waiter.take_outcome() {
            self.registration.take();
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}

async fn wait_for_dispatch_readable<T>(
    stream: &T,
    _io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> std::io::Result<()>
where
    T: DispatchStream,
{
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    if let (Some(runtime), Some(raw_fd)) = (_io_uring_runtime, stream.io_uring_fd()) {
        return runtime.wait_for_fd(raw_fd, Interest::READABLE).await;
    }

    stream.wait_for_readable().await
}

#[cfg(test)]
async fn wait_for_dispatch_writable<T>(
    stream: &T,
    _io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> std::io::Result<()>
where
    T: DispatchStream,
{
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    if let (Some(runtime), Some(raw_fd)) = (_io_uring_runtime, stream.io_uring_fd()) {
        return runtime.wait_for_fd(raw_fd, Interest::WRITABLE).await;
    }

    stream.wait_for_writable().await
}

async fn wait_for_dispatch_readable_or_writable<T>(
    stream: &T,
    _io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> std::io::Result<DispatchReadySide>
where
    T: DispatchStream,
{
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    if let (Some(runtime), Some(raw_fd)) = (_io_uring_runtime, stream.io_uring_fd()) {
        runtime
            .wait_for_fd(raw_fd, Interest::READABLE | Interest::WRITABLE)
            .await?;
        return match stream.try_readable_without_consuming()? {
            DispatchReadinessHint::Ready => Ok(DispatchReadySide::Readable),
            DispatchReadinessHint::NotReady => Ok(DispatchReadySide::Writable),
            DispatchReadinessHint::Unsupported => stream.wait_for_readable_or_writable().await,
        };
    }

    stream.wait_for_readable_or_writable().await
}

#[derive(Debug)]
struct PendingOutboundBatch {
    bytes: Vec<u8>,
    reservations: Vec<OutboundReservation>,
    offset: usize,
    transient_retries: usize,
    phase: PendingOutboundPhase,
    prefer_read: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingOutboundPhase {
    Writing,
    Flushing,
}

impl PendingOutboundBatch {
    fn remaining(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundService {
    Readable,
    Progress,
    Complete,
    Terminal,
}

fn reweight_topology_batch(
    terminal: &DispatchTerminal,
    reservations: &mut [OutboundReservation],
    retained_bytes: usize,
) -> anyhow::Result<()> {
    if !reservations
        .iter()
        .any(|reservation| reservation.is_topology)
    {
        return Ok(());
    }
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    match OutboundBudget::reweight_topology_batch(reservations, retained_bytes) {
        Ok(()) => {
            drop(admission);
            Ok(())
        }
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            metrics::counter!(
                "mux.dispatch.outbound_budget.rejected",
                "class" => OutboundClass::Topology.label(),
                "limit" => limit.label(),
            )
            .increment(1);
            anyhow::bail!(
                "encoded mux topology delivery exceeded the {} bound",
                limit.label()
            )
        }
    }
}

fn encode_write_payload(
    payload: WritePayload,
    compression_mode: codec::CompressionMode,
    terminal: &DispatchTerminal,
) -> anyhow::Result<EncodedOutboundFrame> {
    match payload {
        WritePayload::Encoded(frame) => Ok(frame),
        WritePayload::Typed(ReservedDecodedPdu {
            decoded,
            reservation,
        }) => {
            let bytes = decoded
                .pdu
                .encode_frame_with_mode(decoded.serial, compression_mode)
                .context("encoding PDU frame")?;
            let mut reservation = reservation;
            let encoded_capacity = bytes.capacity();
            let grows_topology_charge =
                reservation.is_topology && encoded_capacity > reservation.topology_bytes;
            if grows_topology_charge {
                reweight_topology_batch(
                    terminal,
                    std::slice::from_mut(&mut reservation),
                    encoded_capacity,
                )?;
            }
            drop(decoded);
            if !grows_topology_charge {
                reweight_topology_batch(
                    terminal,
                    std::slice::from_mut(&mut reservation),
                    encoded_capacity,
                )?;
            }
            Ok(EncodedOutboundFrame { bytes, reservation })
        }
    }
}

fn prepare_pending_outbound_batch(
    first: WritePayload,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    compression_mode: codec::CompressionMode,
    terminal: &DispatchTerminal,
) -> anyhow::Result<PendingOutboundBatch> {
    debug_assert!(deferred_item.is_none());
    let EncodedOutboundFrame {
        mut bytes,
        reservation,
    } = encode_write_payload(first, compression_mode, terminal)?;
    let mut reservations = vec![reservation];
    let mut frames = 1_usize;

    while frames < OUTBOUND_WRITE_QUANTUM_FRAMES
        && bytes.len() < OUTBOUND_WRITE_QUANTUM_BYTES
    {
        let payload = match item_rx.try_recv() {
            Ok(Item::WritePdu(payload)) => payload,
            Ok(other) => {
                *deferred_item = Some(other);
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        };
        let frame = encode_write_payload(payload, compression_mode, terminal)?;
        if bytes
            .len()
            .checked_add(frame.bytes.len())
            .is_none_or(|next_len| next_len > OUTBOUND_WRITE_QUANTUM_BYTES)
        {
            *deferred_item = Some(Item::WritePdu(WritePayload::Encoded(frame)));
            break;
        }

        bytes.extend_from_slice(&frame.bytes);
        reservations.push(frame.reservation);
        reweight_topology_batch(terminal, &mut reservations, bytes.capacity())?;
        frames = frames.saturating_add(1);
    }

    metrics::histogram!("mux.dispatch.outbound_batch.frames").record(frames as f64);
    metrics::histogram!("mux.dispatch.outbound_batch.bytes").record(bytes.len() as f64);
    if bytes.len() > OUTBOUND_WRITE_QUANTUM_BYTES {
        metrics::histogram!("mux.dispatch.outbound_batch.overshoot_bytes")
            .record(bytes.len().saturating_sub(OUTBOUND_WRITE_QUANTUM_BYTES) as f64);
    }

    Ok(PendingOutboundBatch {
        bytes,
        reservations,
        offset: 0,
        transient_retries: 0,
        phase: PendingOutboundPhase::Writing,
        prefer_read: true,
    })
}

enum ImmediateOutboundPoll<T> {
    Terminal,
    Pending,
    Ready(io::Result<T>),
}

async fn poll_dispatch_write_once<T>(
    stream: &mut T,
    bytes: &[u8],
    terminal: &DispatchTerminal,
) -> ImmediateOutboundPoll<usize>
where
    T: DispatchStream,
{
    std::future::poll_fn(|cx| {
        let outcome = {
            let Some(_admission) = terminal.admit() else {
                return Poll::Ready(ImmediateOutboundPoll::Terminal);
            };
            match Pin::new(&mut *stream).poll_write(cx, bytes) {
                Poll::Ready(result) => ImmediateOutboundPoll::Ready(result),
                Poll::Pending => ImmediateOutboundPoll::Pending,
            }
        };
        Poll::Ready(outcome)
    })
    .await
}

async fn poll_dispatch_flush_once<T>(
    stream: &mut T,
    terminal: &DispatchTerminal,
) -> ImmediateOutboundPoll<()>
where
    T: DispatchStream,
{
    std::future::poll_fn(|cx| {
        let outcome = {
            let Some(_admission) = terminal.admit() else {
                return Poll::Ready(ImmediateOutboundPoll::Terminal);
            };
            match Pin::new(&mut *stream).poll_flush(cx) {
                Poll::Ready(result) => ImmediateOutboundPoll::Ready(result),
                Poll::Pending => ImmediateOutboundPoll::Pending,
            }
        };
        Poll::Ready(outcome)
    })
    .await
}

async fn service_pending_outbound<T>(
    stream: &mut T,
    pending: &mut PendingOutboundBatch,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
    terminal: &DispatchTerminal,
) -> anyhow::Result<OutboundService>
where
    T: DispatchStream,
{
    let Some(admission) = terminal.admit() else {
        return Ok(OutboundService::Terminal);
    };
    drop(admission);
    if pending.prefer_read
        && stream
            .try_readable_without_consuming()
            .context("probing mux stream readability during an outbound frame")?
            == DispatchReadinessHint::Ready
    {
        // Alternate when both directions remain continuously ready.
        pending.prefer_read = false;
        return Ok(OutboundService::Readable);
    }

    let mut operation_polled_pending = false;
    match pending.phase {
        PendingOutboundPhase::Writing => {
            let turn_len = pending.remaining().len().min(OUTBOUND_WRITE_QUANTUM_BYTES);
            let turn_end = pending.offset.saturating_add(turn_len);
            match poll_dispatch_write_once(
                stream,
                &pending.bytes[pending.offset..turn_end],
                terminal,
            )
            .await
            {
                ImmediateOutboundPoll::Terminal => return Ok(OutboundService::Terminal),
                ImmediateOutboundPoll::Ready(Ok(0)) => {
                    return Err(std::io::Error::new(
                        ErrorKind::WriteZero,
                        "failed to write mux PDU frame chunk",
                    )
                    .into());
                }
                ImmediateOutboundPoll::Ready(Ok(written)) => {
                    pending.offset = pending.offset.saturating_add(written);
                    pending.transient_retries = 0;
                    pending.prefer_read = true;
                    metrics::histogram!("mux.dispatch.outbound_chunk.bytes").record(written as f64);
                    if pending.offset == pending.bytes.len() {
                        pending.phase = PendingOutboundPhase::Flushing;
                    }
                    return Ok(OutboundService::Progress);
                }
                ImmediateOutboundPoll::Pending => operation_polled_pending = true,
                ImmediateOutboundPoll::Ready(Err(err))
                    if is_transient_write_error(&err)
                        && pending.transient_retries < TRANSIENT_WRITE_RETRY_LIMIT =>
                {
                    pending.transient_retries = pending.transient_retries.saturating_add(1);
                }
                ImmediateOutboundPoll::Ready(Err(err)) => return Err(err.into()),
            }
        }
        PendingOutboundPhase::Flushing => match poll_dispatch_flush_once(stream, terminal).await {
            ImmediateOutboundPoll::Terminal => return Ok(OutboundService::Terminal),
            ImmediateOutboundPoll::Ready(Ok(())) => return Ok(OutboundService::Complete),
            ImmediateOutboundPoll::Pending => operation_polled_pending = true,
            ImmediateOutboundPoll::Ready(Err(err))
                if is_transient_write_error(&err)
                    && pending.transient_retries < TRANSIENT_WRITE_RETRY_LIMIT =>
            {
                pending.transient_retries = pending.transient_retries.saturating_add(1);
            }
            ImmediateOutboundPoll::Ready(Err(err)) => return Err(err.into()),
        },
    }

    if operation_polled_pending && stream.pending_outbound_requires_retry() {
        // OpenSSL requires the exact SSL_write/flush operation to be retried
        // with identical arguments before any SSL_read. Preserve the
        // operation-specific interest armed by that poll.
        stream
            .wait_for_pending_outbound_retry()
            .await
            .context("waiting to retry a transport-bound outbound operation")?;
        pending.prefer_read = false;
        return Ok(OutboundService::Progress);
    }

    // The one-shot write/flush poll registered its precise transport
    // interest but did not suspend this task. Replace it with one combined
    // interest so newly readable input can preempt a blocked outbound window.
    let ready_side = wait_for_dispatch_readable_or_writable(stream, io_uring_runtime)
        .await
        .context("waiting after a pending mux stream write or flush")?;
    if ready_side == DispatchReadySide::Readable {
        // Keep write preference false so the next service turn attempts
        // outbound progress before accepting another continuously-ready read.
        pending.prefer_read = false;
        return Ok(OutboundService::Readable);
    }
    Ok(OutboundService::Progress)
}

#[cfg(test)]
async fn write_pending_pdus<T>(
    stream: &mut T,
    first: Box<DecodedPdu>,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    write_pending_pdus_with_compression_mode(
        stream,
        first,
        item_rx,
        deferred_item,
        io_uring_runtime,
        codec::CompressionMode::Auto,
    )
    .await
}

#[cfg(test)]
async fn write_pending_pdus_with_compression_mode<T>(
    stream: &mut T,
    first: Box<DecodedPdu>,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
    compression_mode: codec::CompressionMode,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    wait_for_dispatch_writable(stream, io_uring_runtime)
        .await
        .context("waiting for mux stream to become writable")?;

    let mut current = Some(test_write_payload(first));
    while let Some(payload) = current.take() {
        let (frame, _reservation) = match payload {
            WritePayload::Typed(ReservedDecodedPdu {
                decoded,
                reservation,
            }) => (
                decoded
                    .pdu
                    .encode_frame_with_mode(decoded.serial, compression_mode)
                    .context("encoding PDU frame")?,
                reservation,
            ),
            WritePayload::Encoded(EncodedOutboundFrame { bytes, reservation }) => {
                (bytes, reservation)
            }
        };
        write_frame_with_transient_retries(stream, &frame, io_uring_runtime)
            .await
            .context("encoding PDU to client")?;

        match item_rx.try_recv() {
            Ok(Item::WritePdu(next)) => current = Some(next),
            Ok(other) => {
                *deferred_item = Some(other);
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    stream.flush().await.context("flushing PDU to client")
}

/// Choose between inbound socket work and the internal outbound/notification
/// queue without allowing either ready source to starve the other.
///
/// When both sides remain ready, `prefer_read` alternates turns. The outbound
/// turn itself is bounded by `OUTBOUND_WRITE_QUANTUM_*`, so continuous pane
/// output has a finite service bound before the next inbound readiness probe.
async fn next_dispatch_item<T>(
    stream: &T,
    item_rx: &Receiver<Item>,
    deferred_item: &mut Option<Item>,
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
    prefer_read: &mut bool,
) -> anyhow::Result<Item>
where
    T: DispatchStream,
{
    if *prefer_read {
        match stream
            .try_readable_without_consuming()
            .context("probing mux stream readability")?
        {
            DispatchReadinessHint::Ready => {
                *prefer_read = false;
                return Ok(Item::Readable);
            }
            DispatchReadinessHint::NotReady | DispatchReadinessHint::Unsupported => {}
        }
    }

    if let Some(item) = deferred_item.take() {
        *prefer_read = !matches!(item, Item::Readable);
        return Ok(item);
    }
    match item_rx.try_recv() {
        Ok(item) => {
            *prefer_read = !matches!(item, Item::Readable);
            return Ok(item);
        }
        Err(TryRecvError::Closed) => {
            return Err(anyhow::anyhow!("mux dispatch item queue closed"));
        }
        Err(TryRecvError::Empty) => {}
    }

    let rx_msg = item_rx
        .recv()
        .map(|result| result.map_err(|err| anyhow::anyhow!("{err:?}")));
    let wait_for_read = wait_for_dispatch_readable(stream, io_uring_runtime)
        .map(|result| result.map(|()| Item::Readable).map_err(anyhow::Error::from));
    pin_mut!(rx_msg);
    pin_mut!(wait_for_read);

    let item = if *prefer_read {
        match select(wait_for_read, rx_msg).await {
            Either::Left((result, _)) | Either::Right((result, _)) => result?,
        }
    } else {
        match select(rx_msg, wait_for_read).await {
            Either::Left((result, _)) | Either::Right((result, _)) => result?,
        }
    };
    *prefer_read = !matches!(item, Item::Readable);
    Ok(item)
}

fn is_transient_write_error(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock)
}

#[cfg(test)]
async fn write_frame_with_transient_retries<T>(
    stream: &mut T,
    frame: &[u8],
    io_uring_runtime: Option<&DispatchIoUringRuntime>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    let mut offset = 0;
    let mut transient_retries = 0;
    while offset < frame.len() {
        match stream.write(&frame[offset..]).await {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write complete mux PDU frame",
                )
                .into());
            }
            Ok(written) => {
                offset += written;
                transient_retries = 0;
            }
            Err(err)
                if is_transient_write_error(&err)
                    && transient_retries < TRANSIENT_WRITE_RETRY_LIMIT =>
            {
                transient_retries += 1;
                wait_for_dispatch_writable(stream, io_uring_runtime)
                    .await
                    .context("waiting to retry transient mux stream write failure")?;
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_with_config(stream, DispatchRuntimeConfig::default()).await
}

pub async fn process_with_config<T>(stream: T, config: DispatchRuntimeConfig) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_async_with_config(stream, config).await
}

pub async fn process_unix_auto_with_config(
    stream: UnixStream,
    config: DispatchRuntimeConfig,
) -> anyhow::Result<()> {
    process_auto_with_config(stream, config).await
}

async fn process_auto_with_config<T>(stream: T, config: DispatchRuntimeConfig) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    let mux = Mux::try_get().context("mux singleton is not available")?;
    match detect_incoming_protocol(stream).await? {
        IncomingProtocol::CleanDisconnect => Ok(()),
        IncomingProtocol::BinaryPdu(stream) => process_async_with_mux(stream, config, mux).await,
        IncomingProtocol::TmuxControl { stream, prefix } => {
            process_tmux_control_stream(stream, prefix, mux).await
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ProtocolProbe {
    NeedMore,
    BinaryPdu,
    TmuxControl,
}

enum IncomingProtocol<T>
where
    T: DispatchStream,
{
    CleanDisconnect,
    BinaryPdu(PrefetchedDispatchStream<T>),
    TmuxControl { stream: T, prefix: Vec<u8> },
}

#[derive(Debug)]
struct PrefetchedDispatchStream<T>
where
    T: DispatchStream,
{
    inner: T,
    pending: VecDeque<u8>,
}

impl<T> PrefetchedDispatchStream<T>
where
    T: DispatchStream,
{
    fn new(inner: T, pending: Vec<u8>) -> Self {
        Self {
            inner,
            pending: pending.into(),
        }
    }
}

impl<T> DispatchStream for PrefetchedDispatchStream<T>
where
    T: DispatchStream,
{
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        self.inner.dispatch_stream_kind()
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        if self.pending.is_empty() {
            self.inner.wait_for_readable()
        } else {
            Box::pin(async { Ok(()) })
        }
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        self.inner.wait_for_writable()
    }

    fn wait_for_readable_or_writable(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<DispatchReadySide>> + Send + '_>> {
        if self.pending.is_empty() {
            self.inner.wait_for_readable_or_writable()
        } else {
            Box::pin(async { Ok(DispatchReadySide::Readable) })
        }
    }

    fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
        if self.pending.is_empty() {
            self.inner.try_readable_without_consuming()
        } else {
            Ok(DispatchReadinessHint::Ready)
        }
    }

    fn pending_outbound_requires_retry(&self) -> bool {
        self.inner.pending_outbound_requires_retry()
    }

    fn wait_for_pending_outbound_retry(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        self.inner.wait_for_pending_outbound_retry()
    }

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    fn io_uring_fd(&self) -> Option<RawFd> {
        self.inner.io_uring_fd()
    }
}

impl<T> AsyncRead for PrefetchedDispatchStream<T>
where
    T: DispatchStream,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut drained_pending = false;
        while buf.remaining() > 0 {
            let Some(byte) = this.pending.pop_front() else {
                break;
            };
            buf.put_slice(&[byte]);
            drained_pending = true;
        }
        if drained_pending {
            return std::task::Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for PrefetchedDispatchStream<T>
where
    T: DispatchStream,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

async fn detect_incoming_protocol<T>(mut stream: T) -> anyhow::Result<IncomingProtocol<T>>
where
    T: DispatchStream,
{
    let mut prefix = Vec::new();
    loop {
        wait_for_dispatch_readable(&stream, None)
            .await
            .context("waiting to probe incoming mux socket protocol")?;
        let mut byte = [0u8; 1];
        let read = stream
            .read(&mut byte)
            .await
            .context("probing incoming mux socket protocol")?;
        if read == 0 {
            if prefix.is_empty() {
                return Ok(IncomingProtocol::CleanDisconnect);
            }
            return Ok(IncomingProtocol::BinaryPdu(PrefetchedDispatchStream::new(
                stream, prefix,
            )));
        }
        prefix.push(byte[0]);
        match classify_protocol_probe(&prefix) {
            ProtocolProbe::NeedMore => {}
            ProtocolProbe::BinaryPdu => {
                return Ok(IncomingProtocol::BinaryPdu(PrefetchedDispatchStream::new(
                    stream, prefix,
                )));
            }
            ProtocolProbe::TmuxControl => {
                return Ok(IncomingProtocol::TmuxControl { stream, prefix });
            }
        }
    }
}

fn classify_protocol_probe(bytes: &[u8]) -> ProtocolProbe {
    let Some((&first, rest)) = bytes.split_first() else {
        return ProtocolProbe::NeedMore;
    };
    if !first.is_ascii_alphabetic() {
        return ProtocolProbe::BinaryPdu;
    }

    if bytes.contains(&b'\n') {
        if tmux_control_probe_prefix_is_text(bytes) {
            return ProtocolProbe::TmuxControl;
        }
        return ProtocolProbe::BinaryPdu;
    }

    if !tmux_control_probe_prefix_is_text(rest) {
        return ProtocolProbe::BinaryPdu;
    }

    if bytes.len() >= TMUX_CONTROL_MAX_LINE_BYTES {
        ProtocolProbe::BinaryPdu
    } else {
        ProtocolProbe::NeedMore
    }
}

fn tmux_control_probe_prefix_is_text(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(text) => text
            .chars()
            .all(|c| c == '\n' || c == '\r' || c == '\t' || !c.is_control()),
        Err(err) => err.error_len().is_none(),
    }
}

async fn process_tmux_control_stream<T>(
    mut stream: T,
    mut buffer: Vec<u8>,
    mux: Arc<Mux>,
) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    let mut command_id = 0u64;
    loop {
        while let Some(line_end) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=line_end).collect::<Vec<_>>();
            command_id = command_id.saturating_add(1);
            let response = tmux_control_response_for_line_bytes(
                &mux,
                current_unix_timestamp_secs(),
                command_id,
                line,
            );
            write_tmux_response(&mut stream, response).await?;
        }

        if buffer.len() > TMUX_CONTROL_MAX_LINE_BYTES {
            command_id = command_id.saturating_add(1);
            let response = tmux_error_response(
                current_unix_timestamp_secs(),
                command_id,
                "tmux control command exceeded maximum line length",
            );
            write_tmux_response(&mut stream, response).await?;
            return Ok(());
        }

        wait_for_dispatch_readable(&stream, None)
            .await
            .context("waiting for tmux control command line")?;
        let mut chunk = [0u8; 1024];
        let read = stream
            .read(&mut chunk)
            .await
            .context("reading tmux control command line")?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn tmux_control_response_for_line_bytes(
    mux: &Mux,
    timestamp_secs: u64,
    command_id: u64,
    line: Vec<u8>,
) -> TmuxResponse {
    match String::from_utf8(line) {
        Ok(line) => tmux_control_response_at(mux, timestamp_secs, command_id, &line),
        Err(_) => tmux_error_response(
            timestamp_secs,
            command_id,
            "parse error: invalid utf-8 in tmux control command",
        ),
    }
}

async fn write_tmux_response<T>(stream: &mut T, response: TmuxResponse) -> anyhow::Result<()>
where
    T: DispatchStream,
{
    stream
        .write_all(response.encode().as_bytes())
        .await
        .context("writing tmux control response")?;
    stream
        .flush()
        .await
        .context("flushing tmux control response")
}

fn current_unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn tmux_control_response_at(
    mux: &Mux,
    timestamp_secs: u64,
    command_id: u64,
    line: &str,
) -> TmuxResponse {
    match parse_command(line) {
        Ok(command) => tmux_command_response_at(mux, timestamp_secs, command_id, command),
        Err(err) => tmux_error_response(timestamp_secs, command_id, &format!("parse error: {err}")),
    }
}

fn tmux_command_response_at(
    mux: &Mux,
    timestamp_secs: u64,
    command_id: u64,
    command: TmuxCommand,
) -> TmuxResponse {
    match command {
        TmuxCommand::SendKeys { target, keys } => match tmux_dispatch_send_keys(mux, target, &keys)
        {
            Ok(output) => tmux_success_response(timestamp_secs, command_id, output),
            Err(err) => tmux_error_response(timestamp_secs, command_id, &err),
        },
        TmuxCommand::ListSessions => tmux_success_response(
            timestamp_secs,
            command_id,
            tmux_control_list_sessions_output(mux),
        ),
        TmuxCommand::ListWindows { target_session } => {
            match tmux_control_list_windows_output(mux, target_session.as_deref()) {
                Ok(output) => tmux_success_response(timestamp_secs, command_id, output),
                Err(err) => tmux_error_response(timestamp_secs, command_id, &err),
            }
        }
        TmuxCommand::CapturePane { target, print } => {
            match tmux_dispatch_capture_pane(mux, target, print) {
                Ok(output) => tmux_success_response(timestamp_secs, command_id, output),
                Err(err) => tmux_error_response(timestamp_secs, command_id, &err),
            }
        }
        TmuxCommand::Unknown { verb, .. } => tmux_error_response(
            timestamp_secs,
            command_id,
            &format!("unsupported command: {verb}"),
        ),
        command => tmux_error_response(
            timestamp_secs,
            command_id,
            &format!(
                "unsupported command in native tmux dispatcher: {}",
                tmux_command_name(&command)
            ),
        ),
    }
}

fn tmux_control_target_pane_id(target: Option<&str>) -> Result<usize, String> {
    let target = target.ok_or_else(|| {
        "missing target pane; native tmux dispatcher currently requires -t %<pane_id>".to_string()
    })?;
    let pane_id = target.strip_prefix('%').unwrap_or(target);
    if pane_id.is_empty() || !pane_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("unsupported pane target; expected -t %<pane_id>".to_string());
    }
    pane_id
        .parse::<usize>()
        .map_err(|_| "pane target is too large for this platform".to_string())
}

fn tmux_dispatch_send_keys(
    mux: &Mux,
    target: Option<String>,
    keys: &[String],
) -> Result<Vec<String>, String> {
    let pane_id = tmux_control_target_pane_id(target.as_deref())?;
    let payload = tmux_send_keys_payload(keys);
    let registration = mux
        .capture_current_pane(pane_id)
        .ok_or_else(|| format!("pane not found: %{pane_id}"))?;
    registration
        .try_with_current(|current| {
            current
                .write_all_and_flush(&payload)
                .map_err(|err| format!("send-keys write failed or flush failed: {err}"))?;
            Ok(Vec::new())
        })
        .ok_or_else(|| format!("pane registration is no longer current: %{pane_id}"))?
}

fn tmux_send_keys_payload(keys: &[String]) -> Vec<u8> {
    let mut payload = Vec::new();
    for key in keys {
        match key.as_str() {
            "Enter" | "Return" => payload.push(b'\r'),
            "Space" => payload.push(b' '),
            "Tab" => payload.push(b'\t'),
            "Escape" | "Esc" => payload.push(0x1b),
            "BSpace" | "Backspace" => payload.push(0x7f),
            literal => {
                if let Some(byte) = tmux_control_key_byte(literal) {
                    payload.push(byte);
                } else {
                    payload.extend_from_slice(literal.as_bytes());
                }
            }
        }
    }
    payload
}

fn tmux_control_key_byte(key: &str) -> Option<u8> {
    let name = key.strip_prefix("C-")?;
    let mut chars = name.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii() {
        return None;
    }
    match ch {
        '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        ascii if ascii.is_ascii_alphabetic() => Some(ascii.to_ascii_uppercase() as u8 - b'@'),
        _ => None,
    }
}

fn tmux_dispatch_capture_pane(
    mux: &Mux,
    target: Option<String>,
    print: bool,
) -> Result<Vec<String>, String> {
    if !print {
        return Err("capture-pane without -p is unsupported by native tmux dispatcher".to_string());
    }
    let pane_id = tmux_control_target_pane_id(target.as_deref())?;
    let registration = mux
        .capture_current_pane(pane_id)
        .ok_or_else(|| format!("pane not found: %{pane_id}"))?;
    registration
        .try_with_current(|current| {
            let dimensions = current.get_dimensions();
            let row_count = dimensions
                .scrollback_rows
                .saturating_add(dimensions.viewport_rows);
            let row_end = isize::try_from(row_count).unwrap_or(isize::MAX);
            let (_first_row, lines) = current.get_lines(0..row_end);
            Ok(lines
                .into_iter()
                .map(|line| line.columns_as_str(0..usize::MAX).trim_end().to_string())
                .collect())
        })
        .ok_or_else(|| format!("pane registration is no longer current: %{pane_id}"))?
}

fn tmux_command_name(command: &TmuxCommand) -> &'static str {
    match command {
        TmuxCommand::SendKeys { .. } => "send-keys",
        TmuxCommand::ListWindows { .. } => "list-windows",
        TmuxCommand::ListSessions => "list-sessions",
        TmuxCommand::CapturePane { .. } => "capture-pane",
        TmuxCommand::SplitWindow { .. } => "split-window",
        TmuxCommand::NewSession { .. } => "new-session",
        TmuxCommand::AttachSession { .. } => "attach-session",
        TmuxCommand::Detach => "detach",
        TmuxCommand::PipePane { .. } => "pipe-pane",
        TmuxCommand::CopyMode { .. } => "copy-mode",
        TmuxCommand::Unknown { .. } => "unknown",
    }
}

fn tmux_success_response(
    timestamp_secs: u64,
    command_id: u64,
    output: Vec<String>,
) -> TmuxResponse {
    TmuxResponse {
        timestamp_secs,
        command_id,
        flags: 0,
        output,
        outcome: Ok(()),
    }
}

fn tmux_error_response(timestamp_secs: u64, command_id: u64, message: &str) -> TmuxResponse {
    TmuxResponse {
        timestamp_secs,
        command_id,
        flags: 0,
        output: vec![message.to_string()],
        outcome: Err(message.to_string()),
    }
}

fn tmux_control_list_sessions_output(mux: &Mux) -> Vec<String> {
    let active_workspace = mux.active_workspace();
    let mut workspaces = mux.iter_workspaces();
    if workspaces.is_empty() {
        workspaces.push(active_workspace.clone());
    }
    workspaces.sort();
    workspaces.dedup();

    workspaces
        .into_iter()
        .map(|workspace| {
            let window_count = mux.iter_windows_in_workspace(&workspace).len();
            let attached = if workspace == active_workspace {
                " (attached)"
            } else {
                ""
            };
            format!("{workspace}: {window_count} windows{attached}")
        })
        .collect()
}

fn tmux_control_list_windows_output(
    mux: &Mux,
    target_session: Option<&str>,
) -> Result<Vec<String>, String> {
    let active_workspace;
    let workspace = match target_session {
        Some(workspace) => workspace,
        None => {
            active_workspace = mux.active_workspace();
            active_workspace.as_str()
        }
    };
    let window_ids = mux.iter_windows_in_workspace(workspace);
    if target_session.is_some() && window_ids.is_empty() {
        return Err(format!("session not found: {workspace}"));
    }

    let mut lines = Vec::new();
    for window_id in window_ids {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let active_tab_id = window.get_active().map(|tab| tab.tab_id());
        for (idx, tab) in window.iter().enumerate() {
            let size = tab.get_size();
            let title = tab.get_title();
            let active = if Some(tab.tab_id()) == active_tab_id {
                "*"
            } else {
                ""
            };
            lines.push(format!(
                "{idx}: {title}{active} ({} panes) [{}x{}] @{}",
                tab.iter_panes().len(),
                size.cols,
                size.rows,
                tab.tab_id()
            ));
        }
    }

    Ok(lines)
}

pub async fn process_async<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    process_async_with_config(stream, DispatchRuntimeConfig::default()).await
}

async fn process_async_with_config<T>(
    stream: T,
    config: DispatchRuntimeConfig,
) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    let mux = Mux::try_get().context("mux singleton is not available")?;
    process_async_with_mux(stream, config, mux).await
}

fn dispatch_client_request(
    handler: &mut SessionHandler,
    topology: &TopologyStreamCoordinator,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    if decoded.serial == 0 {
        metrics::counter!(
            "mux.dispatch.protocol_error",
            "reason" => "reserved_request_serial_zero"
        )
        .increment(1);
        anyhow::bail!("mux client request used reserved server-unilateral serial zero");
    }

    if let Pdu::ListPanesCoherent(request) = &decoded.pdu {
        topology.begin_fence(decoded.serial, request)?;
    }
    handler.process_one(decoded);
    Ok(())
}

async fn process_async_with_mux<T>(
    mut stream: T,
    config: DispatchRuntimeConfig,
    mux: Arc<Mux>,
) -> anyhow::Result<()>
where
    T: 'static,
    T: DispatchStream,
{
    let reactor = DispatchReactor::resolve(config, stream.dispatch_stream_kind());
    if let Some(reason) = reactor.fallback_reason() {
        log::trace!(
            "process_async configured backend {:?} resolved to {:?}: {}",
            config.preference(),
            reactor.backend(),
            reason
        );
    } else {
        log::trace!(
            "process_async configured backend {:?} resolved to {:?}",
            config.preference(),
            reactor.backend()
        );
    }

    let (item_tx, item_rx) = bounded::<Item>(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
    let (terminal, terminal_rx) = DispatchTerminal::channel();
    let mut deferred_item = None;
    let mut pending_outbound = None;
    let mut prefer_read = true;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    let io_uring_runtime = DispatchIoUringRuntime::maybe_new(reactor, stream.io_uring_fd());
    #[cfg(not(all(feature = "io-uring", target_os = "linux")))]
    let io_uring_runtime: Option<DispatchIoUringRuntime> = None;

    let owner = SessionOwner::new(mux);
    let authority = owner.authority();
    let mux = Arc::clone(owner.mux());
    let topology_stream_id =
        TopologyStreamId::from_bytes(*uuid::Uuid::new_v4().as_bytes());
    let topology = Arc::new(TopologyStreamCoordinator::new(
        item_tx.clone(),
        terminal.clone(),
        topology_stream_id,
    ));
    let pdu_sender = PduSender::new({
        let topology = Arc::clone(&topology);
        move |pdu| topology.queue_response(pdu)
    });
    let mut handler = SessionHandler::new_for_session_with_topology_stream(
        pdu_sender,
        owner,
        topology_stream_id,
    );

    {
        let notification_authority = authority.clone();
        let notification_mux = Arc::clone(&mux);
        let notification_topology = Arc::clone(&topology);
        let (sub_id, session_incarnation, baseline_revision) = mux
            .subscribe_with_topology_fence(move |envelope| {
                notification_authority
                    .try_run(|| {
                        notification_topology
                            .on_notification(&notification_mux, envelope)
                    })
                    .unwrap_or(false)
            })
            .context("allocate fenced mux dispatch subscription")?;
        let _subscription_guard = MuxSubscriptionGuard::new(Arc::clone(&mux), sub_id)
            .bind_topology(&topology, session_incarnation, baseline_revision)
            .context("bind fenced mux dispatch subscription")?;

        loop {
            let next_item = if let Some(pending) = pending_outbound.as_mut() {
                let outbound_result = {
                    let terminal_event = terminal_rx.recv();
                    let outbound = service_pending_outbound(
                        &mut stream,
                        pending,
                        io_uring_runtime.as_ref(),
                        &terminal,
                    );
                    pin_mut!(terminal_event);
                    pin_mut!(outbound);
                    match select(terminal_event, outbound).await {
                        Either::Left((Ok(reason), _)) => {
                            return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                        }
                        Either::Left((Err(_), _)) => {
                            return Err(anyhow::anyhow!(
                                "mux dispatch terminal channel closed unexpectedly"
                            ));
                        }
                        Either::Right((result, _)) => result,
                    }
                };
                match outbound_result {
                    Ok(OutboundService::Readable) => Ok(Item::Readable),
                    Ok(OutboundService::Progress) => continue,
                    Ok(OutboundService::Complete) => {
                        pending_outbound = None;
                        continue;
                    }
                    Ok(OutboundService::Terminal) => {
                        let reason = terminal_rx
                            .try_recv()
                            .ok()
                            .unwrap_or("mux dispatch connection entered terminal state");
                        return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                    }
                    Err(err) => Err(err),
                }
            } else {
                let terminal_event = terminal_rx.recv();
                let dispatch_item = next_dispatch_item(
                    &stream,
                    &item_rx,
                    &mut deferred_item,
                    io_uring_runtime.as_ref(),
                    &mut prefer_read,
                );
                pin_mut!(terminal_event);
                pin_mut!(dispatch_item);
                match select(terminal_event, dispatch_item).await {
                    Either::Left((Ok(reason), _)) => {
                        return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                    }
                    Either::Left((Err(_), _)) => {
                        return Err(anyhow::anyhow!(
                            "mux dispatch terminal channel closed unexpectedly"
                        ));
                    }
                    Either::Right((result, _)) => result,
                }
            };

            match next_item {
                Ok(Item::Readable) => {
                    let decoded = match Pdu::decode_async(&mut stream, None).await {
                        Ok(data) => data,
                        Err(err) => {
                            if is_clean_disconnect(&err) {
                                // Client disconnected: no need to make a noise.
                                return Ok(());
                            }
                            return Err(err).context("reading Pdu from client");
                        }
                    };
                    dispatch_client_request(&mut handler, &topology, decoded)?;
                }
                Ok(Item::WritePdu(decoded)) => {
                    match prepare_pending_outbound_batch(
                        decoded,
                        &item_rx,
                        &mut deferred_item,
                        codec::CompressionMode::Auto,
                        &terminal,
                    ) {
                        Ok(pending) => {
                            pending_outbound = Some(pending);
                        }
                        Err(err) => return Err(err),
                    }
                }
                Ok(Item::Notif(queued)) => {
                    let ReservedNotification {
                        notification,
                        reservation,
                    } = queued;
                    match notification {
                        MuxNotification::PaneOutput(pane_id) => {
                            handler.schedule_tracked_pane_push(pane_id);
                        }
                        MuxNotification::PaneAdded(_pane_id) => {}
                        MuxNotification::PaneRemoved(pane_id) => {
                            handler.remove_per_pane(pane_id);
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::PaneRemoved(codec::PaneRemoved { pane_id }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::Alert { pane_id, alert } => {
                            // ft-12e8l: use the non-inserting accessor. If the pane
                            // was already removed, re-inserting a fresh PerPane here
                            // would leak because no later PaneRemoved can retire it.
                            if let Some(per_pane) = handler.per_pane_if_present(pane_id) {
                                {
                                    let mut per_pane = per_pane.lock().map_err(|err| {
                                        anyhow::anyhow!("per-pane lock poisoned: {err}")
                                    })?;
                                    per_pane.notifications.push(alert);
                                }
                                handler.schedule_tracked_pane_push(pane_id);
                            }
                        }
                        MuxNotification::SaveToDownloads { .. } => {}
                        MuxNotification::AssignClipboard {
                            pane_id,
                            selection,
                            clipboard,
                        } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::SetClipboard(codec::SetClipboard {
                                    pane_id,
                                    clipboard,
                                    selection,
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::TabAddedToWindow { tab_id, window_id } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabAddedToWindow(codec::TabAddedToWindow {
                                    tab_id,
                                    window_id,
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::WindowRemoved(_window_id)
                        | MuxNotification::WindowCreated(_window_id)
                        | MuxNotification::WindowInvalidated(_window_id) => {}
                        MuxNotification::WindowWorkspaceChanged {
                            window_id,
                            workspace,
                        } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                                    window_id,
                                    workspace,
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::PaneFocused(pane_id) => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::PaneFocused(codec::PaneFocused { pane_id }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::TabResized(tab_id) => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabResized(codec::TabResized { tab_id }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::TabTitleChanged { tab_id, title } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::WindowTitleChanged { window_id, title } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::WindowTitleChanged(codec::WindowTitleChanged {
                                    window_id,
                                    title,
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::WorkspaceRenamed {
                            old_workspace,
                            new_workspace,
                        } => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::RenameWorkspace(codec::RenameWorkspace {
                                    old_workspace,
                                    new_workspace,
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::SynchronizedOutput { .. }
                        | MuxNotification::ActiveWorkspaceChanged(_)
                        | MuxNotification::Empty => {}
                    }
                }
                Err(err) => {
                    if is_clean_disconnect(&err) {
                        return Ok(());
                    }
                    return Err(err).context("waiting for mux stream readiness or dispatch item");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
    use async_channel::unbounded;
    use codec::{CompressionMode, Ping, Pong, SendPaste, WriteToPane};
    use mux::domain::DomainId;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use parking_lot::{MappedMutexGuard, Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::io;
    use std::io::Cursor;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    use std::io::Write;
    use std::ops::Range;
    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use termwiz::surface::{Line, SequenceNo};
    use wezterm_term::color::ColorPalette;
    use wezterm_term::terminal::{Alert, ClipboardSelection};
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: &Arc<Mux>) -> Self {
            let lock = crate::GLOBAL_STATE_TEST_LOCK
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = Mux::try_get();
            Mux::set_mux(mux);
            Self { prior, _lock: lock }
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    struct CapturingPane {
        pane_id: usize,
        lines: ParkingMutex<Vec<Line>>,
        writes: ParkingMutex<Vec<u8>>,
        dimensions: RenderableDimensions,
        mux_registration: Arc<mux::PaneRegistrationSlot>,
    }

    impl CapturingPane {
        fn new(pane_id: usize, lines: &[&str]) -> Self {
            let line_count = lines.len();
            Self {
                pane_id,
                lines: ParkingMutex::new(
                    lines
                        .iter()
                        .map(|line| Line::from_text(line, &Default::default(), 1, None))
                        .collect(),
                ),
                writes: ParkingMutex::new(Vec::new()),
                mux_registration: Arc::new(mux::PaneRegistrationSlot::default()),
                dimensions: RenderableDimensions {
                    cols: 80,
                    viewport_rows: line_count,
                    scrollback_rows: 0,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: 96,
                    pixel_width: 800,
                    pixel_height: 600,
                    reverse_video: false,
                },
            }
        }

        fn written_bytes(&self) -> Vec<u8> {
            self.writes.lock().clone()
        }
    }

    impl Pane for CapturingPane {
        fn pane_id(&self) -> usize {
            self.pane_id
        }

        fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
            &self.mux_registration
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            1
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let start = usize::try_from(lines.start.max(0)).unwrap_or(usize::MAX);
            let end = usize::try_from(lines.end.max(lines.start).max(0)).unwrap_or(usize::MAX);
            (
                lines.start,
                self.lines
                    .lock()
                    .iter()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .cloned()
                    .collect(),
            )
        }

        fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
            mux::pane::impl_with_lines_via_get_lines(self, lines, with_lines);
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line);
        }

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            self.dimensions
        }

        fn get_title(&self) -> String {
            "capture-pane-test".to_string()
        }

        fn send_paste(&self, text: &str) -> anyhow::Result<()> {
            self.writes.lock().extend_from_slice(text.as_bytes());
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            ParkingMutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }

        fn resize(&self, _size: TerminalSize) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            DomainId::default()
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<url::Url> {
            None
        }
    }

    fn install_mux_with_pane(pane: &Arc<dyn Pane>) -> (Arc<Mux>, ScopedMux) {
        let mux = Arc::new(Mux::new(None));
        let guard = ScopedMux::install(&mux);
        mux.add_pane(pane).unwrap();
        (mux, guard)
    }

    fn capturing_pdu_sender() -> (PduSender, Arc<ParkingMutex<Vec<DecodedPdu>>>) {
        let captured = Arc::new(ParkingMutex::new(Vec::new()));
        let captured_for_sender = Arc::clone(&captured);
        let sender = PduSender::new(move |pdu| {
            captured_for_sender.lock().push(pdu);
            Ok(())
        });
        (sender, captured)
    }

    fn idle_topology_coordinator() -> TopologyStreamCoordinator {
        let (item_tx, _item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, _terminal_rx) = DispatchTerminal::channel();
        TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x5a; 16]),
        )
    }

    fn bound_topology_coordinator() -> (
        TopologyStreamCoordinator,
        Receiver<Item>,
        Receiver<&'static str>,
        MuxSessionIncarnation,
        TopologyStreamId,
    ) {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x5a; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa5; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::INITIAL)
            .expect("bind test topology subscription");
        (
            coordinator,
            item_rx,
            terminal_rx,
            session_incarnation,
            stream_id,
        )
    }

    fn fenced_snapshot_request() -> ListPanesCoherent {
        ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        }
    }

    fn coherent_snapshot_response(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: TopologyRevision,
    ) -> ListPanesCoherentResponse {
        ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id,
            outcome: ListPanesCoherentOutcome::Snapshot(codec::CoherentPaneSnapshot {
                session_incarnation,
                snapshot_revision,
                panes: codec::ListPanesResponse {
                    tabs: Vec::new(),
                    tab_titles: Vec::new(),
                    window_titles: std::collections::HashMap::new(),
                },
            }),
        }
    }

    fn topology_envelope(
        revision: u64,
        notification: MuxNotification,
    ) -> MuxNotificationEnvelope {
        MuxNotificationEnvelope {
            notification,
            topology: MuxTopologyStamp::Revision(TopologyRevision::new(revision)),
        }
    }

    fn retain_topology_for_test(
        coordinator: &TopologyStreamCoordinator,
        notification: MuxNotification,
        revision: TopologyRevision,
    ) -> RetainedTopologyEvent {
        retained_topology_event(
            notification,
            revision,
            &coordinator.terminal,
            &coordinator.outbound_budget,
        )
        .expect("test topology notification should fit its connection budget")
    }

    #[test]
    fn retained_workspace_event_uses_its_mutation_point_payload() {
        let coordinator = idle_topology_coordinator();
        let retained = retain_topology_for_test(
            &coordinator,
            MuxNotification::WindowWorkspaceChanged {
                window_id: 17,
                workspace: "workspace-at-revision".to_string(),
            },
            TopologyRevision::new(9),
        );

        assert_eq!(retained.revision, TopologyRevision::new(9));
        let event = into_topology_event_kind(retained.notification)
            .expect("retained workspace notification should become a stamped event");
        assert!(matches!(
            event,
            TopologyEventKind::WindowWorkspaceChanged {
                window_id: 17,
                workspace: Some(ref workspace),
            } if workspace == "workspace-at-revision"
        ));
    }

    #[test]
    fn retained_dynamic_topology_events_charge_allocated_capacity() {
        let coordinator = idle_topology_coordinator();
        let fixed_bytes = RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES;

        let mut workspace = String::with_capacity(128);
        workspace.push_str("ws");
        let workspace_capacity = workspace.capacity();

        let mut tab_title = String::with_capacity(256);
        tab_title.push_str("tab");
        let tab_title_capacity = tab_title.capacity();

        let mut window_title = String::with_capacity(512);
        window_title.push_str("window");
        let window_title_capacity = window_title.capacity();

        let mut old_workspace = String::with_capacity(1_024);
        old_workspace.push_str("old");
        let mut new_workspace = String::with_capacity(2_048);
        new_workspace.push_str("new");
        let renamed_capacity = old_workspace
            .capacity()
            .checked_add(new_workspace.capacity())
            .expect("small test capacities should add without overflow");

        let cases = [
            (
                MuxNotification::WindowWorkspaceChanged {
                    window_id: 1,
                    workspace,
                },
                workspace_capacity,
                TopologyEventKind::WindowWorkspaceChanged {
                    window_id: 1,
                    workspace: Some("ws".to_string()),
                },
            ),
            (
                MuxNotification::TabTitleChanged {
                    tab_id: 2,
                    title: tab_title,
                },
                tab_title_capacity,
                TopologyEventKind::TabTitleChanged {
                    tab_id: 2,
                    title: "tab".to_string(),
                },
            ),
            (
                MuxNotification::WindowTitleChanged {
                    window_id: 3,
                    title: window_title,
                },
                window_title_capacity,
                TopologyEventKind::WindowTitleChanged {
                    window_id: 3,
                    title: "window".to_string(),
                },
            ),
            (
                MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                },
                renamed_capacity,
                TopologyEventKind::WorkspaceRenamed {
                    old_workspace: "old".to_string(),
                    new_workspace: "new".to_string(),
                },
            ),
        ];

        for (revision, (notification, dynamic_capacity, expected_event)) in
            (1_u64..).zip(cases)
        {
            let retained = retain_topology_for_test(
                &coordinator,
                notification,
                TopologyRevision::new(revision),
            );
            assert_eq!(
                retained.retained_bytes,
                fixed_bytes
                    .checked_add(dynamic_capacity)
                    .expect("small test capacities should add without overflow")
            );
            assert_eq!(
                into_topology_event_kind(retained.notification)
                    .expect("retained notification should become a stamped event"),
                expected_event
            );
        }
    }

    #[test]
    fn retained_large_title_is_held_once_and_moved_into_stamped_event() {
        const TITLE_BYTES: usize = 2_100_000;
        let (coordinator, item_rx, _terminal_rx, _, stream_id) = bound_topology_coordinator();

        let title = "x".repeat(TITLE_BYTES);
        let title_allocation = title.as_ptr();
        let title_capacity = title.capacity();
        let retained = retain_topology_for_test(
            &coordinator,
            MuxNotification::WindowTitleChanged {
                window_id: 17,
                title,
            },
            TopologyRevision::new(4),
        );
        assert_eq!(
            retained.retained_bytes,
            RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES + title_capacity
        );
        assert!(retained.retained_bytes < TOPOLOGY_FENCE_MAX_RETAINED_BYTES);

        let mut buffer = TopologyEventBuffer::default();
        buffer
            .insert(retained)
            .expect("one roughly 2.1 MiB title should fit the retained-byte budget");
        let retained = buffer
            .remove(TopologyRevision::new(4))
            .expect("removing retained title should maintain byte accounting")
            .expect("large title should remain buffered");
        assert_eq!(buffer.retained_bytes, 0);
        assert!(buffer.events.is_empty());

        coordinator
            .queue_stamped_event(retained)
            .expect("retained title should queue as a stamped topology event");
        let decoded = take_written_pdu(&item_rx);
        assert_eq!(decoded.serial, 0);
        let Pdu::TopologyEvent(event) = decoded.pdu else {
            panic!("retained title should queue as a topology event");
        };
        assert_eq!(event.stream_id, stream_id);
        assert_eq!(event.revision, TopologyRevision::new(4));
        let TopologyEventKind::WindowTitleChanged { title, .. } = event.event else {
            panic!("retained window title changed variant unexpectedly");
        };
        assert_eq!(title.len(), TITLE_BYTES);
        assert_eq!(title.capacity(), title_capacity);
        assert_eq!(title.as_ptr(), title_allocation);
    }

    fn take_written_pdu(item_rx: &Receiver<Item>) -> DecodedPdu {
        match item_rx.try_recv().expect("queued dispatch item") {
            Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu { decoded, .. })) => *decoded,
            Item::WritePdu(WritePayload::Encoded(_)) => {
                panic!("expected queued typed PDU, got an encoded frame")
            }
            other => panic!("expected queued PDU, got {other:?}"),
        }
    }

    #[test]
    fn protocol_probe_recognizes_tmux_control_line() {
        assert_eq!(
            classify_protocol_probe(b"list-windows"),
            ProtocolProbe::NeedMore
        );
        assert_eq!(
            classify_protocol_probe(b"list-windows -t dev\n"),
            ProtocolProbe::TmuxControl
        );
    }

    #[test]
    fn protocol_probe_recognizes_utf8_tmux_control_line() {
        assert_eq!(
            classify_protocol_probe(b"send-keys caf\xc3\xa9\n"),
            ProtocolProbe::TmuxControl
        );
    }

    #[test]
    fn protocol_probe_waits_for_partial_utf8_tmux_control_line() {
        assert_eq!(
            classify_protocol_probe(b"send-keys caf\xc3"),
            ProtocolProbe::NeedMore
        );
        assert_eq!(
            classify_protocol_probe(b"send-keys caf\xc3\xa9"),
            ProtocolProbe::NeedMore
        );
    }

    #[test]
    fn protocol_probe_keeps_binary_pdu_prefixes_on_pdu_path() {
        assert_eq!(classify_protocol_probe(&[0]), ProtocolProbe::BinaryPdu);
        assert_eq!(classify_protocol_probe(b"d\0"), ProtocolProbe::BinaryPdu);
        assert_eq!(
            classify_protocol_probe(b"send-keys caf\xc3(\n"),
            ProtocolProbe::BinaryPdu
        );
    }

    #[test]
    fn tmux_control_invalid_utf8_lines_are_framed_errors() {
        let mux = Mux::new(None);
        let response =
            tmux_control_response_for_line_bytes(&mux, 10, 7, b"send-keys caf\xc3(\n".to_vec());
        let encoded = response.encode();

        assert!(encoded.contains("parse error: invalid utf-8 in tmux control command"));
        assert!(encoded.ends_with("%error 10 7 0\n"));
    }

    #[test]
    fn tmux_control_parse_errors_are_framed_errors() {
        let mux = Mux::new(None);
        let response = tmux_control_response_at(&mux, 10, 7, "send-keys \"unterminated\n");
        let encoded = response.encode();

        assert!(encoded.contains("parse error:"));
        assert!(encoded.ends_with("%error 10 7 0\n"));
    }

    #[test]
    fn tmux_control_unknown_commands_return_tmux_error_frames() {
        let mux = Mux::new(None);
        let response = tmux_control_response_at(&mux, 11, 8, "kill-server\n");
        let encoded = response.encode();

        assert!(encoded.contains("unsupported command: kill-server"));
        assert!(encoded.ends_with("%error 11 8 0\n"));
    }

    #[test]
    fn tmux_control_typed_tier_two_commands_return_safe_tmux_error_frames() {
        let mux = Mux::new(None);
        let response = tmux_control_response_at(&mux, 11, 8, "pipe-pane -o 'cat >/tmp/out'\n");
        let encoded = response.encode();

        assert!(encoded.contains("unsupported command in native tmux dispatcher: pipe-pane"));
        assert!(!encoded.contains("cat >/tmp/out"));
        assert!(encoded.ends_with("%error 11 8 0\n"));

        let response = tmux_control_response_at(&mux, 12, 9, "copy-mode -t %1 -u\n");
        let encoded = response.encode();

        assert!(encoded.contains("unsupported command in native tmux dispatcher: copy-mode"));
        assert!(!encoded.contains("%1"));
        assert!(encoded.ends_with("%error 12 9 0\n"));
    }

    #[test]
    fn tmux_control_unsupported_tier_one_does_not_echo_payload() {
        let mux = Mux::new(None);
        let response = tmux_control_response_at(&mux, 11, 8, "send-keys secret-token Enter\n");
        let encoded = response.encode();

        assert!(encoded.contains("missing target pane"));
        assert!(!encoded.contains("secret-token"));
        assert!(encoded.ends_with("%error 11 8 0\n"));
    }

    #[test]
    fn tmux_control_list_sessions_reports_mux_workspaces() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let window = mux.new_empty_window(Some("dev".to_string()), None);
        drop(window);

        let response = tmux_control_response_at(&mux, 12, 9, "list-sessions\n");

        assert!(response.outcome.is_ok());
        assert!(
            response.output.iter().any(|line| line == "dev: 1 windows"),
            "{:?}",
            response.output
        );
    }

    #[test]
    fn tmux_control_list_windows_missing_session_is_error() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);

        let response = tmux_control_response_at(&mux, 13, 10, "list-windows -t missing\n");

        assert!(response.outcome.is_err());
        assert_eq!(response.output, vec!["session not found: missing"]);
    }

    #[test]
    fn tmux_control_capture_pane_prints_live_pane_lines() {
        let pane = Arc::new(CapturingPane::new(42, &["alpha", "beta"]));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (mux, _guard) = install_mux_with_pane(&pane_dyn);

        let response = tmux_control_response_at(&mux, 14, 11, "capture-pane -p -t %42\n");

        assert!(response.outcome.is_ok());
        assert_eq!(response.output, vec!["alpha", "beta"]);
        assert!(response.encode().ends_with("%end 14 11 0\n"));
    }

    #[test]
    fn tmux_control_capture_pane_requires_print_mode() {
        let pane = Arc::new(CapturingPane::new(42, &["alpha"]));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (mux, _guard) = install_mux_with_pane(&pane_dyn);

        let response = tmux_control_response_at(&mux, 14, 11, "capture-pane -t %42\n");

        assert!(response.outcome.is_err());
        assert_eq!(
            response.output,
            vec!["capture-pane without -p is unsupported by native tmux dispatcher"]
        );
    }

    #[test]
    fn tmux_control_send_keys_writes_live_pane_input() {
        let pane = Arc::new(CapturingPane::new(42, &[]));
        let pane_dyn: Arc<dyn Pane> = pane.clone();
        let (mux, _guard) = install_mux_with_pane(&pane_dyn);

        let response =
            tmux_control_response_at(&mux, 14, 11, "send-keys -t %42 echo Space hi Enter C-c\n");

        assert!(response.outcome.is_ok());
        assert!(response.output.is_empty());
        assert_eq!(pane.written_bytes(), b"echo hi\r\x03");
    }

    #[test]
    fn tmux_control_commands_remain_bound_to_origin_mux_after_global_replacement() {
        let origin_pane = Arc::new(CapturingPane::new(43, &[]));
        let origin_pane_dyn: Arc<dyn Pane> = origin_pane.clone();
        let origin = Arc::new(Mux::new(None));
        origin.add_pane(&origin_pane_dyn).unwrap();

        let replacement_pane = Arc::new(CapturingPane::new(43, &[]));
        let replacement_pane_dyn: Arc<dyn Pane> = replacement_pane.clone();
        let replacement = Arc::new(Mux::new(None));
        replacement.add_pane(&replacement_pane_dyn).unwrap();

        let _guard = ScopedMux::install(&origin);
        Mux::set_mux(&replacement);

        let response = tmux_control_response_at(&origin, 14, 12, "send-keys -t %43 origin Enter\n");

        assert!(response.outcome.is_ok());
        assert_eq!(origin_pane.written_bytes(), b"origin\r");
        assert!(
            replacement_pane.written_bytes().is_empty(),
            "a live tmux-control connection must never redirect through a replacement singleton"
        );
    }

    #[test]
    fn tmux_control_send_keys_rejects_non_pane_target_without_payload_echo() {
        let mux = Mux::new(None);
        let response = tmux_control_response_at(
            &mux,
            14,
            11,
            "send-keys -t session:window secret-token Enter\n",
        );
        let encoded = response.encode();

        assert!(response.outcome.is_err());
        assert!(encoded.contains("unsupported pane target"));
        assert!(!encoded.contains("secret-token"));
        assert!(encoded.ends_with("%error 14 11 0\n"));
    }

    #[derive(Debug, Default)]
    struct EofDispatchStream;

    impl DispatchStream for EofDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for EofDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for EofDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct PartialFrameDisconnectStream {
        frame_prefix: Vec<u8>,
        cursor: usize,
        chunk_size: usize,
    }

    impl PartialFrameDisconnectStream {
        fn new(frame_prefix: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                frame_prefix,
                cursor: 0,
                chunk_size: chunk_size.max(1),
            }
        }
    }

    impl DispatchStream for PartialFrameDisconnectStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for PartialFrameDisconnectStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let available = this.frame_prefix.len().saturating_sub(this.cursor);
            if available == 0 {
                return Poll::Ready(Ok(()));
            }
            let want = buf.remaining().min(available).min(this.chunk_size);
            if want == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&this.frame_prefix[this.cursor..this.cursor + want]);
            this.cursor += want;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PartialFrameDisconnectStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn subscription_guard_eagerly_unsubscribes_on_drop() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::clone(&observed);
        let sub_id = mux
            .subscribe(move |_| {
                notifications.fetch_add(1, Ordering::Relaxed);
                true
            })
            .expect("test mux subscription should allocate an identifier");

        {
            let _guard = MuxSubscriptionGuard::new(Arc::clone(&mux), sub_id);
            mux.notify(MuxNotification::Empty);
            assert_eq!(observed.load(Ordering::Relaxed), 1);
        }

        assert!(
            !mux.unsubscribe(sub_id),
            "subscription guard should remove the subscriber eagerly"
        );
        mux.notify(MuxNotification::Empty);
        assert_eq!(observed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn subscription_guard_unsubscribes_when_topology_binding_fails() {
        let mux = Arc::new(Mux::new(None));
        let (sub_id, session_incarnation, baseline_revision) = mux
            .subscribe_with_topology_fence(|_| true)
            .expect("test fenced mux subscription should allocate an identifier");
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let coordinator = TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x69; 16]),
        );
        coordinator.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);

        let result = MuxSubscriptionGuard::new(Arc::clone(&mux), sub_id).bind_topology(
            &coordinator,
            session_incarnation,
            baseline_revision,
        );
        let Err(error) = result else {
            panic!("terminal topology binding must fail");
        };
        assert!(
            error
                .to_string()
                .contains("mux dispatch connection is already terminal"),
            "unexpected topology binding failure: {error:#}"
        );
        assert!(
            !mux.unsubscribe(sub_id),
            "failed topology binding must drop the guard and remove its subscriber"
        );
        assert_eq!(
            terminal_rx.try_recv().expect("binding terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());
    }

    #[test]
    fn dispatch_client_request_rejects_reserved_zero_before_handler() {
        let mux = Arc::new(Mux::new(None));
        let (sender, captured) = capturing_pdu_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));
        let topology = idle_topology_coordinator();

        let error = dispatch_client_request(
            &mut handler,
            &topology,
            DecodedPdu {
                serial: 0,
                pdu: Pdu::SetClientId(codec::SetClientId {
                    client_id: mux::client::ClientId::new(),
                    is_proxy: false,
                }),
            },
        )
        .expect_err("client request serial zero must be a hard protocol error");

        assert!(
            format!("{error:#}").contains("reserved server-unilateral serial zero"),
            "serial-zero rejection should retain its fixed protocol classification: {error:#}"
        );
        assert!(
            captured.lock().is_empty(),
            "serial-zero rejection must not enqueue a response that the client would treat as unilateral"
        );
        assert!(
            mux.iter_clients().is_empty(),
            "serial-zero SetClientId must be rejected before session mutation"
        );
    }

    #[test]
    fn dispatch_client_request_delegates_nonzero_serial_unchanged() {
        let mux = Arc::new(Mux::new(None));
        let (sender, captured) = capturing_pdu_sender();
        let mut handler = SessionHandler::new_for_mux(sender, mux);
        let topology = idle_topology_coordinator();

        dispatch_client_request(
            &mut handler,
            &topology,
            DecodedPdu {
                serial: 1,
                pdu: Pdu::Ping(Ping {}),
            },
        )
        .expect("nonzero client request serial should reach the session handler");

        let captured = captured.lock();
        assert_eq!(
            captured.len(),
            1,
            "Ping should produce exactly one response"
        );
        assert_eq!(captured[0].serial, 1);
        assert_eq!(captured[0].pdu, Pdu::Pong(Pong {}));
    }

    #[test]
    fn topology_fence_queues_snapshot_before_reordered_contiguous_events() {
        let (coordinator, item_rx, _terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(41, &fenced_snapshot_request())
            .expect("begin coherent topology fence");

        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(2, MuxNotification::PaneRemoved(2)),
        ));
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));
        assert!(
            item_rx.is_empty(),
            "events remain quarantined until the coherent snapshot response"
        );

        coordinator
            .queue_response(DecodedPdu {
                serial: 41,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::INITIAL,
                )),
            })
            .expect("complete coherent topology fence");

        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 41);
        assert!(matches!(
            snapshot.pdu,
            Pdu::ListPanesCoherentResponse(_)
        ));
        for expected_revision in [1, 2] {
            let event = take_written_pdu(&item_rx);
            assert_eq!(event.serial, 0);
            let Pdu::TopologyEvent(event) = event.pdu else {
                panic!("expected stamped topology event");
            };
            assert_eq!(event.stream_id, stream_id);
            assert_eq!(event.revision, TopologyRevision::new(expected_revision));
        }
        assert!(item_rx.is_empty());
    }

    #[test]
    fn established_topology_stream_holds_a_gap_until_its_missing_revision_arrives() {
        let (coordinator, item_rx, _terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(42, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        coordinator
            .queue_response(DecodedPdu {
                serial: 42,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::INITIAL,
                )),
            })
            .expect("establish topology stream");
        let _snapshot = take_written_pdu(&item_rx);

        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(2, MuxNotification::PaneRemoved(2)),
        ));
        assert!(
            item_rx.is_empty(),
            "revision two must remain quarantined while revision one is missing"
        );
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));

        for expected_revision in [1, 2] {
            let event = take_written_pdu(&item_rx);
            let Pdu::TopologyEvent(event) = event.pdu else {
                panic!("expected stamped topology event");
            };
            assert_eq!(event.revision, TopologyRevision::new(expected_revision));
        }
        assert!(item_rx.is_empty());
    }

    #[test]
    fn coherent_snapshot_prunes_only_buffered_revisions_at_or_before_its_fence() {
        let (coordinator, item_rx, _terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(43, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(2, MuxNotification::PaneRemoved(2)),
        ));
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));

        coordinator
            .queue_response(DecodedPdu {
                serial: 43,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::new(1),
                )),
            })
            .expect("complete coherent topology fence");

        let _snapshot = take_written_pdu(&item_rx);
        let event = take_written_pdu(&item_rx);
        let Pdu::TopologyEvent(event) = event.pdu else {
            panic!("expected post-snapshot topology event");
        };
        assert_eq!(event.revision, TopologyRevision::new(2));
        assert!(item_rx.is_empty());
    }

    #[test]
    fn coherent_snapshot_cannot_predate_its_subscription_baseline() {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x5b; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa6; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::new(5))
            .expect("bind topology subscription at revision five");
        coordinator
            .begin_fence(44, &fenced_snapshot_request())
            .expect("begin coherent topology fence");

        let error = coordinator
            .queue_response(DecodedPdu {
                serial: 44,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::new(4),
                )),
            })
            .expect_err("a snapshot older than subscription admission must fail closed");

        assert!(
            error
                .to_string()
                .contains("did not match the connection subscription")
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("pre-baseline snapshot must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(
            item_rx.is_empty(),
            "an invalid snapshot must not become observable on the wire"
        );
    }

    #[test]
    fn contended_snapshot_restores_every_quarantined_legacy_notification() {
        let (coordinator, item_rx, _terminal_rx, _session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(44, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(2, MuxNotification::PaneRemoved(2)),
        ));
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));

        coordinator
            .queue_response(DecodedPdu {
                serial: 44,
                pdu: Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                    negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                    stream_id,
                    outcome: ListPanesCoherentOutcome::Contended {
                        attempts: 3,
                        first_revision: TopologyRevision::INITIAL,
                        last_revision: TopologyRevision::new(2),
                    },
                }),
            })
            .expect("return typed snapshot contention");

        let response = take_written_pdu(&item_rx);
        assert!(matches!(
            response.pdu,
            Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                outcome: ListPanesCoherentOutcome::Contended { .. },
                ..
            })
        ));
        assert!(matches!(
            item_rx.try_recv().expect("first restored notification"),
            Item::Notif(ReservedNotification {
                notification: MuxNotification::PaneAdded(1),
                ..
            })
        ));
        assert!(matches!(
            item_rx.try_recv().expect("second restored notification"),
            Item::Notif(ReservedNotification {
                notification: MuxNotification::PaneRemoved(2),
                ..
            })
        ));
        assert!(item_rx.is_empty());
    }

    #[test]
    fn topology_fence_duplicate_and_capacity_overflow_are_terminal_and_release_retention() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(45, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        let mut retained_prefix =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        retained_prefix.push('p');
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title: retained_prefix,
                },
            ),
        ));
        let mut overflowing_title =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        overflowing_title.push('x');
        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(
                2,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title: overflowing_title,
                },
            ),
        ));
        assert_eq!(
            terminal_rx.try_recv().expect("terminal overflow reason"),
            TOPOLOGY_BUFFER_OVERFLOW
        );
        assert!(item_rx.is_empty());
        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(2, MuxNotification::PaneAdded(2)),
        ));
        assert!(
            terminal_rx.is_empty(),
            "a tripped coordinator must not emit a second terminal transition"
        );
        {
            let state = coordinator.state.lock();
            assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
            assert_eq!(state.prebind.retained_bytes, 0);
            assert!(state.prebind.events.is_empty());
        }
        assert!(
            coordinator
                .begin_fence(99, &fenced_snapshot_request())
                .is_err(),
            "post-terminal fence admission must fail"
        );
        assert!(
            coordinator
                .queue_response(DecodedPdu {
                    serial: 99,
                    pdu: Pdu::Pong(Pong {}),
                })
                .is_err(),
            "post-terminal response admission must fail"
        );
        assert!(item_rx.is_empty());

        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        coordinator
            .begin_fence(46, &fenced_snapshot_request())
            .expect("begin second coherent topology fence");
        coordinator
            .queue_response(DecodedPdu {
                serial: 46,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::INITIAL,
                )),
            })
            .expect("establish second topology stream");
        let _snapshot = take_written_pdu(&item_rx);
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));
        let _first = take_written_pdu(&item_rx);
        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));
        assert_eq!(
            terminal_rx.try_recv().expect("terminal duplicate reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
    }

    #[test]
    fn topology_capacity_overflow_releases_prebind_and_established_gap_retention() {
        let mux = Mux::new(None);

        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x6a; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        let mut retained_prefix =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        retained_prefix.push('p');
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::TabTitleChanged {
                    tab_id: 1,
                    title: retained_prefix,
                },
            ),
        ));
        let mut overflowing_title =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        overflowing_title.push('x');
        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(
                2,
                MuxNotification::TabTitleChanged {
                    tab_id: 1,
                    title: overflowing_title,
                },
            ),
        ));
        assert_eq!(
            terminal_rx.try_recv().expect("prebind overflow reason"),
            TOPOLOGY_BUFFER_OVERFLOW
        );
        assert!(item_rx.is_empty());
        {
            let state = coordinator.state.lock();
            assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
            assert_eq!(state.prebind.retained_bytes, 0);
            assert!(state.prebind.events.is_empty());
        }
        assert!(
            coordinator
                .bind_subscription(
                    MuxSessionIncarnation::from_bytes([0xb5; 16]),
                    TopologyRevision::new(2),
                )
                .is_err(),
            "post-terminal subscription binding must fail"
        );

        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        coordinator
            .begin_fence(47, &fenced_snapshot_request())
            .expect("begin established-gap setup fence");
        coordinator
            .queue_response(DecodedPdu {
                serial: 47,
                pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                    stream_id,
                    session_incarnation,
                    TopologyRevision::INITIAL,
                )),
            })
            .expect("establish topology stream");
        let _snapshot = take_written_pdu(&item_rx);

        let mut retained_prefix =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        retained_prefix.push('p');
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                3,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title: retained_prefix,
                },
            ),
        ));
        let mut overflowing_title =
            String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
        overflowing_title.push('x');
        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(
                4,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title: overflowing_title,
                },
            ),
        ));
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("established gap overflow reason"),
            TOPOLOGY_BUFFER_OVERFLOW
        );
        assert!(item_rx.is_empty());
        let state = coordinator.state.lock();
        assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
        assert_eq!(state.prebind.retained_bytes, 0);
        assert!(state.prebind.events.is_empty());
    }

    #[test]
    fn topology_terminal_transition_is_linearizable_with_late_ingress() {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let coordinator = Arc::new(TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x6b; 16]),
        ));
        coordinator
            .bind_subscription(
                MuxSessionIncarnation::from_bytes([0xb6; 16]),
                TopologyRevision::INITIAL,
            )
            .expect("bind race-test topology subscription");

        let admission_entered = Arc::new(std::sync::Barrier::new(2));
        let release_terminal = Arc::new(std::sync::Barrier::new(2));
        let tripper_terminal = coordinator.terminal.clone();
        let tripper_entered = Arc::clone(&admission_entered);
        let tripper_release = Arc::clone(&release_terminal);
        let tripper = std::thread::spawn(move || {
            let admission = tripper_terminal
                .admit()
                .expect("terminal tripper should acquire live admission");
            tripper_entered.wait();
            tripper_release.wait();
            admission.trip(TOPOLOGY_PROTOCOL_FAILURE);
        });
        admission_entered.wait();

        let late_coordinator = Arc::clone(&coordinator);
        let late_callback_started = Arc::new(std::sync::Barrier::new(2));
        let callback_started = Arc::clone(&late_callback_started);
        let late_callback = std::thread::spawn(move || {
            let mux = Mux::new(None);
            callback_started.wait();
            late_coordinator.on_notification(
                &mux,
                topology_envelope(1, MuxNotification::PaneAdded(1)),
            )
        });
        late_callback_started.wait();
        release_terminal.wait();

        tripper.join().expect("terminal tripper thread should join");
        assert!(
            !late_callback
                .join()
                .expect("late notification thread should join"),
            "ingress linearized after the terminal transition must be rejected"
        );
        assert_eq!(
            terminal_rx.try_recv().expect("linearized terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());
        let state = coordinator.state.lock();
        assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
        assert_eq!(state.prebind.retained_bytes, 0);
        assert!(state.prebind.events.is_empty());
    }

    #[test]
    fn topology_result_admission_rejects_terminal_race_after_early_check() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        coordinator
            .begin_fence(48, &fenced_snapshot_request())
            .expect("begin terminal-race topology fence");
        let mux = Mux::new(None);
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));

        let coordinator = Arc::new(coordinator);
        let operation_entered = Arc::new(std::sync::Barrier::new(2));
        let terminal_published = Arc::new(std::sync::Barrier::new(2));
        let operation_coordinator = Arc::clone(&coordinator);
        let operation_entered_thread = Arc::clone(&operation_entered);
        let terminal_published_thread = Arc::clone(&terminal_published);
        let operation = std::thread::spawn(move || {
            operation_coordinator.with_live_result(|| {
                // Reaching this closure proves the wrapper's early terminal
                // check completed while the connection was still live.
                operation_entered_thread.wait();
                terminal_published_thread.wait();
                Ok(())
            })
        });
        operation_entered.wait();
        coordinator.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
        terminal_published.wait();

        let error = operation
            .join()
            .expect("terminal-race operation thread should join")
            .expect_err("a terminal transition during admission must reject later success");
        assert!(
            error
                .to_string()
                .contains("mux dispatch connection became terminal during admission"),
            "unexpected terminal-race rejection: {:#}",
            error
        );
        assert_eq!(
            terminal_rx.try_recv().expect("terminal-race reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());
        let state = coordinator.state.lock();
        assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
        assert_eq!(state.prebind.retained_bytes, 0);
        assert!(state.prebind.events.is_empty());
    }

    #[test]
    fn topology_preparation_does_not_hold_the_hot_notification_admission_gate() {
        let coordinator = idle_topology_coordinator();
        coordinator
            .with_live_result(|| {
                let _admission = coordinator
                    .terminal
                    .admission
                    .try_lock()
                    .expect("deep topology preparation must not hold the short admission gate");
                Ok(())
            })
            .expect("live topology preparation should complete");
    }

    #[test]
    fn pane_output_enqueue_bypasses_deep_topology_work_before_terminal_transition() {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let coordinator = Arc::new(TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x6c; 16]),
        ));

        let operation_entered = Arc::new(std::sync::Barrier::new(2));
        let release_operation = Arc::new(std::sync::Barrier::new(2));
        let topology_coordinator = Arc::clone(&coordinator);
        let topology_entered = Arc::clone(&operation_entered);
        let topology_release = Arc::clone(&release_operation);
        let topology_operation = std::thread::spawn(move || {
            topology_coordinator.with_live_result(|| {
                let _state = topology_coordinator.state.lock();
                topology_entered.wait();
                topology_release.wait();
                Ok(())
            })
        });
        operation_entered.wait();

        let (callback_done_tx, callback_done_rx) = std::sync::mpsc::sync_channel(1);
        let callback_coordinator = Arc::clone(&coordinator);
        let callback = std::thread::spawn(move || {
            let mux = Mux::new(None);
            let accepted = callback_coordinator.on_notification(
                &mux,
                MuxNotificationEnvelope {
                    notification: MuxNotification::PaneOutput(73),
                    topology: MuxTopologyStamp::NonTopology,
                },
            );
            callback_done_tx
                .send(accepted)
                .expect("report pane-output callback result");
        });

        let accepted = callback_done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("pane output must not wait for deep topology state work");
        assert!(accepted, "live pane output should win admission");

        // The pane output linearized first. The paired late-ingress test proves
        // the opposite order; together they pin the short admission gate while
        // this still-blocked topology operation must lose its final check.
        coordinator.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
        release_operation.wait();
        callback.join().expect("pane-output callback should join");
        let error = topology_operation
            .join()
            .expect("topology operation should join")
            .expect_err("terminal transition must reject the older operation's late result");
        assert!(
            error
                .to_string()
                .contains("mux dispatch connection became terminal during admission"),
            "unexpected topology admission error: {error:#}"
        );
        assert_eq!(
            terminal_rx.try_recv().expect("terminal transition reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(matches!(
            item_rx.try_recv(),
            Ok(Item::Notif(ReservedNotification {
                notification: MuxNotification::PaneOutput(73),
                ..
            }))
        ));
        assert!(item_rx.is_empty());
        let state = coordinator.state.lock();
        assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
        assert_eq!(state.prebind.retained_bytes, 0);
        assert!(state.prebind.events.is_empty());
    }

    #[test]
    fn process_async_treats_unexpected_eof_as_clean_disconnect() {
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(EofDispatchStream));
        assert!(
            result.is_ok(),
            "EOF should be treated as a normal client disconnect"
        );
    }

    #[derive(Debug)]
    struct ReadErrorDispatchStream {
        kind: io::ErrorKind,
    }

    impl DispatchStream for ReadErrorDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for ReadErrorDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(self.kind, "client disconnected")))
        }
    }

    impl AsyncWrite for ReadErrorDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn process_async_treats_read_side_connection_reset_as_clean_disconnect() {
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(ReadErrorDispatchStream {
            kind: io::ErrorKind::ConnectionReset,
        }));
        assert!(
            result.is_ok(),
            "read-side connection reset should be treated as a normal client disconnect"
        );
    }

    #[derive(Debug, Default)]
    struct FailingReadableDispatchStream;

    impl DispatchStream for FailingReadableDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Err(io::Error::other("readable wait failed")) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for FailingReadableDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other(
                "poll_read unexpectedly ran before the readiness error surfaced",
            )))
        }
    }

    impl AsyncWrite for FailingReadableDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other(
                "poll_write unexpectedly ran in the readability failure test",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn process_async_propagates_readable_wait_failures() {
        let mux = Arc::new(Mux::new(None));
        let _scoped_mux = ScopedMux::install(&mux);
        let result = promise::spawn::block_on(process_async(FailingReadableDispatchStream));
        let err = result.expect_err("readable wait failures must not be swallowed");
        let message = format!("{err:#}");
        assert!(
            message.contains("readable wait failed"),
            "error should preserve the readiness failure context: {message}"
        );
    }

    #[derive(Debug, Default)]
    struct CountingDispatchStream {
        bytes_written: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl DispatchStream for CountingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for CountingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for CountingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes_written.fetch_add(buf.len(), Ordering::Relaxed);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug, Default)]
    struct RecordingDispatchStream {
        bytes: Vec<u8>,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl DispatchStream for RecordingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::pending())
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }

        fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
            Ok(DispatchReadinessHint::Ready)
        }
    }

    impl AsyncRead for RecordingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for RecordingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug, Default)]
    struct ChunkedDispatchStream {
        readable: AtomicBool,
        bytes: Vec<u8>,
        write_sizes: Vec<usize>,
    }

    impl DispatchStream for ChunkedDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            if self.readable.load(Ordering::Acquire) {
                Box::pin(async { Ok(()) })
            } else {
                Box::pin(std::future::pending())
            }
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
            Ok(if self.readable.load(Ordering::Acquire) {
                DispatchReadinessHint::Ready
            } else {
                DispatchReadinessHint::NotReady
            })
        }
    }

    #[derive(Debug, Default)]
    struct PendingWriteThenReadableDispatchStream {
        readable: AtomicBool,
        combined_waits: AtomicUsize,
        write_polls: AtomicUsize,
    }

    impl DispatchStream for PendingWriteThenReadableDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::pending())
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::pending())
        }

        fn wait_for_readable_or_writable(
            &self,
        ) -> Pin<Box<dyn Future<Output = io::Result<DispatchReadySide>> + Send + '_>> {
            Box::pin(async move {
                self.combined_waits.fetch_add(1, Ordering::Relaxed);
                self.readable.store(true, Ordering::Release);
                Ok(DispatchReadySide::Readable)
            })
        }

        fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
            Ok(if self.readable.load(Ordering::Acquire) {
                DispatchReadinessHint::Ready
            } else {
                DispatchReadinessHint::NotReady
            })
        }
    }

    impl AsyncRead for PendingWriteThenReadableDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingWriteThenReadableDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write_polls.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug, Default)]
    struct UnsupportedReadinessPendingWriteStream {
        readable_waits: AtomicUsize,
        write_polls: AtomicUsize,
    }

    impl DispatchStream for UnsupportedReadinessPendingWriteStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.readable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    impl AsyncRead for UnsupportedReadinessPendingWriteStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for UnsupportedReadinessPendingWriteStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write_polls.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for ChunkedDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for ChunkedDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes.extend_from_slice(buf);
            this.write_sizes.push(buf.len());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn queued_ping(serial: u64) -> Box<DecodedPdu> {
        Box::new(DecodedPdu {
            pdu: Pdu::Ping(Ping {}),
            serial,
        })
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedOutboundPdu {
        Ping,
        Pong,
        WriteToPane { pane_id: usize, data: Vec<u8> },
        SendPaste { pane_id: usize, data: String },
    }

    impl GeneratedOutboundPdu {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::Ping => Pdu::Ping(Ping {}),
                Self::Pong => Pdu::Pong(Pong {}),
                Self::WriteToPane { pane_id, data } => {
                    Pdu::WriteToPane(WriteToPane { pane_id, data })
                }
                Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste { pane_id, data }),
            }
        }

        fn matches_decoded(&self, decoded: &Pdu) -> bool {
            match (self, decoded) {
                (Self::Ping, Pdu::Ping(Ping {})) | (Self::Pong, Pdu::Pong(Pong {})) => true,
                (
                    Self::WriteToPane { pane_id, data },
                    Pdu::WriteToPane(WriteToPane {
                        pane_id: decoded_pane_id,
                        data: decoded_data,
                    }),
                ) => pane_id == decoded_pane_id && data == decoded_data,
                (
                    Self::SendPaste { pane_id, data },
                    Pdu::SendPaste(SendPaste {
                        pane_id: decoded_pane_id,
                        data: decoded_data,
                    }),
                ) => pane_id == decoded_pane_id && data == decoded_data,
                _ => false,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedMuxNotification {
        Empty,
        PaneRemoved {
            pane_id: usize,
        },
        AlertPaletteChanged {
            pane_id: usize,
        },
        AlertBell {
            pane_id: usize,
        },
        PaneFocused {
            pane_id: usize,
        },
        TabResized {
            tab_id: usize,
        },
        TabTitleChanged {
            tab_id: usize,
            title: String,
        },
        WindowTitleChanged {
            window_id: usize,
            title: String,
        },
        WorkspaceRenamed {
            old_workspace: String,
            new_workspace: String,
        },
        AssignClipboard {
            pane_id: usize,
            selection: ClipboardSelection,
            clipboard: Option<String>,
        },
    }

    impl GeneratedMuxNotification {
        fn into_notification(self) -> MuxNotification {
            match self {
                Self::Empty => MuxNotification::Empty,
                Self::PaneRemoved { pane_id } => MuxNotification::PaneRemoved(pane_id),
                Self::AlertPaletteChanged { pane_id } => MuxNotification::Alert {
                    pane_id,
                    alert: Alert::PaletteChanged,
                },
                Self::AlertBell { pane_id } => MuxNotification::Alert {
                    pane_id,
                    alert: Alert::Bell,
                },
                Self::PaneFocused { pane_id } => MuxNotification::PaneFocused(pane_id),
                Self::TabResized { tab_id } => MuxNotification::TabResized(tab_id),
                Self::TabTitleChanged { tab_id, title } => {
                    MuxNotification::TabTitleChanged { tab_id, title }
                }
                Self::WindowTitleChanged { window_id, title } => {
                    MuxNotification::WindowTitleChanged { window_id, title }
                }
                Self::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                },
                Self::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                },
            }
        }
    }

    fn queued_generated_pdu(serial: u64, pdu: GeneratedOutboundPdu) -> Box<DecodedPdu> {
        Box::new(DecodedPdu {
            pdu: pdu.into_pdu(),
            serial,
        })
    }

    fn leb128_prefix_len(bytes: &[u8]) -> Option<usize> {
        bytes
            .iter()
            .position(|byte| (byte & 0x80) == 0)
            .map(|index| index + 1)
    }

    fn malformed_length_frame(encoded: &[u8], malformed_len: u8) -> Vec<u8> {
        let original_len_prefix =
            leb128_prefix_len(encoded).expect("encoded PDU has length prefix");
        let mut malformed = Vec::with_capacity(encoded.len() - original_len_prefix + 1);
        malformed.push(malformed_len);
        malformed.extend_from_slice(&encoded[original_len_prefix..]);
        malformed
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum NetworkWriteFailure {
        BrokenPipe,
        ConnectionReset,
        NotConnected,
        UnexpectedEof,
        TimedOut,
        WouldBlock,
        Other,
    }

    impl NetworkWriteFailure {
        fn kind(self) -> io::ErrorKind {
            match self {
                Self::BrokenPipe => io::ErrorKind::BrokenPipe,
                Self::ConnectionReset => io::ErrorKind::ConnectionReset,
                Self::NotConnected => io::ErrorKind::NotConnected,
                Self::UnexpectedEof => io::ErrorKind::UnexpectedEof,
                Self::TimedOut => io::ErrorKind::TimedOut,
                Self::WouldBlock => io::ErrorKind::WouldBlock,
                Self::Other => io::ErrorKind::Other,
            }
        }

        const fn is_transient(self) -> bool {
            matches!(self, Self::WouldBlock)
        }

        const fn is_clean_disconnect(self) -> bool {
            matches!(
                self,
                Self::BrokenPipe | Self::ConnectionReset | Self::NotConnected | Self::UnexpectedEof
            )
        }
    }

    fn network_write_failures() -> impl Strategy<Value = NetworkWriteFailure> {
        prop_oneof![
            Just(NetworkWriteFailure::BrokenPipe),
            Just(NetworkWriteFailure::ConnectionReset),
            Just(NetworkWriteFailure::NotConnected),
            Just(NetworkWriteFailure::UnexpectedEof),
            Just(NetworkWriteFailure::TimedOut),
            Just(NetworkWriteFailure::WouldBlock),
            Just(NetworkWriteFailure::Other),
        ]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TransientWriteFailure {
        Interrupted,
        WouldBlock,
    }

    impl TransientWriteFailure {
        const fn kind(self) -> io::ErrorKind {
            match self {
                Self::Interrupted => io::ErrorKind::Interrupted,
                Self::WouldBlock => io::ErrorKind::WouldBlock,
            }
        }
    }

    fn transient_write_failures() -> impl Strategy<Value = TransientWriteFailure> {
        prop_oneof![
            Just(TransientWriteFailure::Interrupted),
            Just(TransientWriteFailure::WouldBlock),
        ]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PartialFrameFailure {
        WriteZero,
        UnexpectedEof,
        BrokenPipe,
        ConnectionReset,
        Other,
    }

    impl PartialFrameFailure {
        fn result(self) -> io::Result<usize> {
            match self {
                Self::WriteZero => Ok(0),
                Self::UnexpectedEof => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "simulated EOF after partial frame write",
                )),
                Self::BrokenPipe => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "simulated broken pipe after partial frame write",
                )),
                Self::ConnectionReset => Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "simulated connection reset after partial frame write",
                )),
                Self::Other => Err(io::Error::other(
                    "simulated non-disconnect error after partial frame write",
                )),
            }
        }

        const fn is_clean_disconnect(self) -> bool {
            matches!(
                self,
                Self::UnexpectedEof | Self::BrokenPipe | Self::ConnectionReset
            )
        }
    }

    fn partial_frame_failures() -> impl Strategy<Value = PartialFrameFailure> {
        prop_oneof![
            Just(PartialFrameFailure::WriteZero),
            Just(PartialFrameFailure::UnexpectedEof),
            Just(PartialFrameFailure::BrokenPipe),
            Just(PartialFrameFailure::ConnectionReset),
            Just(PartialFrameFailure::Other),
        ]
    }

    #[derive(Debug)]
    struct FailingWriteDispatchStream {
        failure: NetworkWriteFailure,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl FailingWriteDispatchStream {
        fn new(failure: NetworkWriteFailure) -> Self {
            Self {
                failure,
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for FailingWriteDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for FailingWriteDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FailingWriteDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Err(io::Error::new(
                this.failure.kind(),
                "simulated network write failure",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct TransientThenSuccessfulWriteDispatchStream {
        transient: TransientWriteFailure,
        failures_remaining: usize,
        bytes: Vec<u8>,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl TransientThenSuccessfulWriteDispatchStream {
        fn new(transient: TransientWriteFailure, failures_before_success: usize) -> Self {
            Self {
                transient,
                failures_remaining: failures_before_success,
                bytes: Vec::new(),
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for TransientThenSuccessfulWriteDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for TransientThenSuccessfulWriteDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for TransientThenSuccessfulWriteDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);
            if this.failures_remaining > 0 {
                this.failures_remaining -= 1;
                return Poll::Ready(Err(io::Error::new(
                    this.transient.kind(),
                    "simulated transient network write failure",
                )));
            }

            this.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct PartialFrameFailingDispatchStream {
        fail_after_bytes: usize,
        failure: PartialFrameFailure,
        bytes: Vec<u8>,
        write_attempts: AtomicUsize,
        flush_calls: AtomicUsize,
        writable_waits: AtomicUsize,
    }

    impl PartialFrameFailingDispatchStream {
        fn new(fail_after_bytes: usize, failure: PartialFrameFailure) -> Self {
            Self {
                fail_after_bytes,
                failure,
                bytes: Vec::new(),
                write_attempts: AtomicUsize::new(0),
                flush_calls: AtomicUsize::new(0),
                writable_waits: AtomicUsize::new(0),
            }
        }
    }

    impl DispatchStream for PartialFrameFailingDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            self.writable_waits.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for PartialFrameFailingDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PartialFrameFailingDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_attempts.fetch_add(1, Ordering::Relaxed);

            if this.bytes.len() < this.fail_after_bytes {
                let remaining_prefix = this.fail_after_bytes - this.bytes.len();
                let accepted = remaining_prefix.min(buf.len());
                this.bytes.extend_from_slice(&buf[..accepted]);
                return Poll::Ready(Ok(accepted));
            }

            Poll::Ready(this.failure.result())
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum QueuedDispatchItem {
        WritePdu,
        Notif,
        Readable,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum QueuedGeneratedDispatchItem {
        WritePdu(GeneratedOutboundPdu),
        Notif,
        Readable,
    }

    fn queued_dispatch_items() -> impl Strategy<Value = Vec<QueuedDispatchItem>> {
        proptest::collection::vec(
            prop_oneof![
                Just(QueuedDispatchItem::WritePdu),
                Just(QueuedDispatchItem::Notif),
                Just(QueuedDispatchItem::Readable),
            ],
            0..=16,
        )
    }

    fn generated_outbound_pdu() -> impl Strategy<Value = GeneratedOutboundPdu> {
        prop_oneof![
            Just(GeneratedOutboundPdu::Ping),
            Just(GeneratedOutboundPdu::Pong),
            (0usize..4096, proptest::collection::vec(any::<u8>(), 0..512))
                .prop_map(|(pane_id, data)| GeneratedOutboundPdu::WriteToPane { pane_id, data },),
            (0usize..4096, "[a-zA-Z0-9 _./-]{0,512}")
                .prop_map(|(pane_id, data)| { GeneratedOutboundPdu::SendPaste { pane_id, data } }),
        ]
    }

    fn compression_modes() -> impl Strategy<Value = CompressionMode> {
        prop_oneof![
            Just(CompressionMode::Auto),
            Just(CompressionMode::Always),
            Just(CompressionMode::Never),
        ]
    }

    fn queued_generated_dispatch_items() -> impl Strategy<Value = Vec<QueuedGeneratedDispatchItem>>
    {
        proptest::collection::vec(
            prop_oneof![
                generated_outbound_pdu().prop_map(QueuedGeneratedDispatchItem::WritePdu),
                Just(QueuedGeneratedDispatchItem::Notif),
                Just(QueuedGeneratedDispatchItem::Readable),
            ],
            0..=16,
        )
    }

    fn clipboard_selections() -> impl Strategy<Value = ClipboardSelection> {
        prop_oneof![
            Just(ClipboardSelection::Clipboard),
            Just(ClipboardSelection::PrimarySelection),
        ]
    }

    fn generated_mux_notification() -> impl Strategy<Value = GeneratedMuxNotification> {
        let label = "[a-zA-Z0-9 _./-]{0,48}";
        prop_oneof![
            Just(GeneratedMuxNotification::Empty),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::PaneRemoved { pane_id }),
            (0usize..4096)
                .prop_map(|pane_id| GeneratedMuxNotification::AlertPaletteChanged { pane_id }),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::AlertBell { pane_id }),
            (0usize..4096).prop_map(|pane_id| GeneratedMuxNotification::PaneFocused { pane_id }),
            (0usize..256).prop_map(|tab_id| GeneratedMuxNotification::TabResized { tab_id }),
            (0usize..256, label).prop_map(|(tab_id, title)| {
                GeneratedMuxNotification::TabTitleChanged { tab_id, title }
            }),
            (0usize..256, label).prop_map(|(window_id, title)| {
                GeneratedMuxNotification::WindowTitleChanged { window_id, title }
            }),
            (label, label).prop_map(|(old_workspace, new_workspace)| {
                GeneratedMuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                }
            }),
            (
                0usize..4096,
                clipboard_selections(),
                prop::option::of(label)
            )
                .prop_map(|(pane_id, selection, clipboard)| {
                    GeneratedMuxNotification::AssignClipboard {
                        pane_id,
                        selection,
                        clipboard,
                    }
                },),
        ]
    }

    fn classify_item(item: &Item) -> QueuedDispatchItem {
        match item {
            Item::WritePdu(_) => QueuedDispatchItem::WritePdu,
            Item::Notif(_) => QueuedDispatchItem::Notif,
            Item::Readable => QueuedDispatchItem::Readable,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn proptest_write_pending_pdus_preserves_serial_order_and_deferred_boundary(
            queued_items in queued_dispatch_items()
        ) {
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_written_serials = vec![1_u64];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(test_write_item(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        if still_in_write_prefix {
                            expected_written_serials.push(next_serial);
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(test_notification_item(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = RecordingDispatchStream::default();
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(1),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            prop_assert!(result.is_ok(), "batched write helper should succeed: {result:?}");
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "batched writes should flush exactly once"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "batched writes should wait for writability exactly once"
            );

            let mut decoded_serials = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                let decoded = Pdu::decode(&mut cursor).expect("decode recorded outbound PDU");
                prop_assert!(
                    matches!(decoded.pdu, Pdu::Ping(_)),
                    "generated dispatch test should only encode Ping PDUs"
                );
                decoded_serials.push(decoded.serial);
            }

            prop_assert_eq!(
                decoded_serials,
                expected_written_serials,
                "write batch must preserve the contiguous outbound PDU serial order"
            );
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "items after the first deferred non-write must stay queued in original order"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_preserves_dispatch_order_across_compression_modes(
            compression_mode in compression_modes(),
            first_pdu in generated_outbound_pdu(),
            queued_items in queued_generated_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let first_serial = u64::from(first_serial);
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = first_serial.saturating_add(1);
            let mut expected_written = vec![(first_serial, first_pdu.clone())];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedGeneratedDispatchItem::WritePdu(pdu) => {
                        item_tx
                            .try_send(test_write_item(queued_generated_pdu(
                                next_serial,
                                pdu.clone(),
                            )))
                            .expect("queue generated PDU");
                        if still_in_write_prefix {
                            expected_written.push((next_serial, pdu));
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial = next_serial.saturating_add(1);
                    }
                    QueuedGeneratedDispatchItem::Notif => {
                        item_tx
                            .try_send(test_notification_item(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedGeneratedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = RecordingDispatchStream::default();
            let result = promise::spawn::block_on(write_pending_pdus_with_compression_mode(
                &mut stream,
                queued_generated_pdu(first_serial, first_pdu),
                &item_rx,
                &mut deferred_item,
                None,
                compression_mode,
            ));

            prop_assert!(
                result.is_ok(),
                "batched write helper should succeed under {:?}: {:?}",
                compression_mode,
                result
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "batched writes should flush exactly once under {:?}",
                compression_mode
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "batched writes should wait for writability exactly once under {:?}",
                compression_mode
            );

            let mut decoded = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                decoded.push(Pdu::decode(&mut cursor).expect("decode recorded outbound PDU"));
            }

            prop_assert_eq!(
                decoded.len(),
                expected_written.len(),
                "compression mode {:?} must not change dispatch frame count",
                compression_mode
            );
            for (decoded, (expected_serial, expected_pdu)) in
                decoded.iter().zip(expected_written.iter())
            {
                prop_assert_eq!(
                    decoded.serial,
                    *expected_serial,
                    "compression mode {:?} changed outbound serial order",
                    compression_mode
                );
                prop_assert!(
                    expected_pdu.matches_decoded(&decoded.pdu),
                    "compression mode {:?} changed outbound PDU payload: expected {:?}, got {:?}",
                    compression_mode,
                    expected_pdu,
                    decoded.pdu
                );
            }
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "compression mode {:?} must preserve queued items after the first deferred non-write",
                compression_mode
            );
        }

        #[test]
        fn proptest_process_async_treats_partial_inbound_frame_disconnect_as_clean(
            compression_mode in compression_modes(),
            inbound_pdu in generated_outbound_pdu(),
            serial in any::<u16>(),
            cut_seed in any::<usize>(),
            chunk_size in 1usize..64,
        ) {
            let mut encoded = Vec::new();
            inbound_pdu
                .clone()
                .into_pdu()
                .encode_with_mode(&mut encoded, u64::from(serial), compression_mode)
                .expect("generated inbound PDU should encode");
            prop_assume!(encoded.len() > 1);
            let cut = 1 + (cut_seed % (encoded.len() - 1));
            let frame_prefix = encoded[..cut].to_vec();

            let mux = Arc::new(Mux::new(None));
            let _scoped_mux = ScopedMux::install(&mux);
            let result = promise::spawn::block_on(process_async(PartialFrameDisconnectStream::new(
                frame_prefix,
                chunk_size,
            )));

            prop_assert!(
                result.is_ok(),
                "partial inbound frame disconnect should be clean for {:?} {:?} cut {}/{} chunk {}: {:?}",
                compression_mode,
                inbound_pdu,
                cut,
                encoded.len(),
                chunk_size,
                result
            );
        }

        #[test]
        fn proptest_process_async_rejects_malformed_inbound_frame_lengths(
            compression_mode in compression_modes(),
            inbound_pdu in generated_outbound_pdu(),
            serial in any::<u16>(),
            malformed_len in 0u8..=1,
            chunk_size in 1usize..64,
        ) {
            let serial = u64::from(serial);
            let mut encoded = Vec::new();
            inbound_pdu
                .clone()
                .into_pdu()
                .encode_with_mode(&mut encoded, serial, compression_mode)
                .expect("generated inbound PDU should encode");
            let malformed = malformed_length_frame(&encoded, malformed_len);

            prop_assert!(
                Pdu::decode(malformed.as_slice()).is_err(),
                "malformed length should not decode synchronously for {:?} {:?} len {}",
                compression_mode,
                inbound_pdu,
                malformed_len
            );

            let mux = Arc::new(Mux::new(None));
            let _scoped_mux = ScopedMux::install(&mux);
            let result = promise::spawn::block_on(process_async(PartialFrameDisconnectStream::new(
                malformed,
                chunk_size,
            )));

            let err = result.expect_err("malformed inbound frame length must fail dispatch");
            let message = format!("{err:#}");
            prop_assert!(
                message.contains("reading Pdu from client"),
                "dispatch should retain inbound PDU read context: {message}"
            );
            prop_assert!(
                message.contains("sizes don't make sense"),
                "malformed length should surface the codec size invariant: {message}"
            );
            prop_assert!(
                !is_clean_disconnect(&err),
                "malformed length must be a hard protocol error, not a clean disconnect"
            );
        }

        #[test]
        fn proptest_notification_error_bus_subscription_liveness_contract(
            notification in generated_mux_notification()
        ) {
            let notification = notification.into_notification();

            let (full_tx, full_rx) = bounded(1);
            let (full_terminal, full_terminal_rx) = DispatchTerminal::channel();
            full_tx
                .try_send(Item::Readable)
                .expect("fill dispatch notification queue");
            prop_assert!(
                !queue_notification(&full_tx, &full_terminal, notification.clone()),
                "full notification queue must terminate the subscription"
            );
            prop_assert!(
                matches!(full_rx.try_recv(), Ok(Item::Readable)),
                "terminal overflow must not displace the older queued item"
            );
            prop_assert!(
                matches!(full_rx.try_recv(), Err(TryRecvError::Empty)),
                "full queue path must not enqueue the unrepresentable notification"
            );
            prop_assert_eq!(
                full_terminal_rx.try_recv().ok(),
                Some(NOTIFICATION_QUEUE_OVERFLOW),
                "notification loss must publish one terminal reason"
            );

            let mux = Mux::new(None);
            let full_calls = Arc::new(AtomicUsize::new(0));
            let full_calls_for_subscriber = Arc::clone(&full_calls);
            let (subscriber_terminal, subscriber_terminal_rx) = DispatchTerminal::channel();
            full_tx
                .try_send(Item::Readable)
                .expect("refill dispatch notification queue");
            mux.subscribe(move |notification| {
                full_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(&full_tx, &subscriber_terminal, notification)
            })
            .expect("test mux subscription should allocate an identifier");
            mux.notify(notification.clone());
            mux.notify(notification.clone());
            prop_assert_eq!(
                full_calls.load(Ordering::Relaxed),
                1,
                "the first lossy enqueue must unsubscribe the mux subscriber"
            );
            prop_assert_eq!(
                subscriber_terminal_rx.try_recv().ok(),
                Some(NOTIFICATION_QUEUE_OVERFLOW),
                "subscriber overflow must trip the connection terminal"
            );

            let (closed_tx, closed_rx) = bounded(1);
            let (closed_terminal, _closed_terminal_rx) = DispatchTerminal::channel();
            drop(closed_rx);
            prop_assert!(
                !queue_notification(&closed_tx, &closed_terminal, notification.clone()),
                "closed notification queue should report a dead subscription"
            );

            let mux = Mux::new(None);
            let closed_calls = Arc::new(AtomicUsize::new(0));
            let closed_calls_for_subscriber = Arc::clone(&closed_calls);
            mux.subscribe(move |notification| {
                closed_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(&closed_tx, &closed_terminal, notification)
            })
            .expect("test mux subscription should allocate an identifier");
            mux.notify(notification.clone());
            mux.notify(notification);
            prop_assert_eq!(
                closed_calls.load(Ordering::Relaxed),
                1,
                "closed notification queue should remove the mux subscriber after the first failed delivery"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_retries_transient_network_errors_without_reordering(
            transient in transient_write_failures(),
            failures_before_success in 1usize..=TRANSIENT_WRITE_RETRY_LIMIT,
            first_pdu in generated_outbound_pdu(),
            queued_items in queued_generated_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let first_serial = u64::from(first_serial);
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = first_serial.saturating_add(1);
            let mut expected_written = vec![(first_serial, first_pdu.clone())];
            let mut expected_deferred = None;
            let mut expected_remaining = Vec::new();
            let mut still_in_write_prefix = true;

            for queued_item in queued_items {
                match queued_item {
                    QueuedGeneratedDispatchItem::WritePdu(pdu) => {
                        item_tx
                            .try_send(test_write_item(queued_generated_pdu(
                                next_serial,
                                pdu.clone(),
                            )))
                            .expect("queue generated PDU");
                        if still_in_write_prefix {
                            expected_written.push((next_serial, pdu));
                        } else {
                            expected_remaining.push(QueuedDispatchItem::WritePdu);
                        }
                        next_serial = next_serial.saturating_add(1);
                    }
                    QueuedGeneratedDispatchItem::Notif => {
                        item_tx
                            .try_send(test_notification_item(MuxNotification::Empty))
                            .expect("queue generated notification");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Notif);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Notif);
                        }
                    }
                    QueuedGeneratedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                        if still_in_write_prefix {
                            expected_deferred = Some(QueuedDispatchItem::Readable);
                            still_in_write_prefix = false;
                        } else {
                            expected_remaining.push(QueuedDispatchItem::Readable);
                        }
                    }
                }
            }

            let mut deferred_item = None;
            let mut stream = TransientThenSuccessfulWriteDispatchStream::new(
                transient,
                failures_before_success,
            );
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_generated_pdu(first_serial, first_pdu),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            prop_assert!(
                result.is_ok(),
                "transient {:?} write failures should retry and preserve dispatch: {:?}",
                transient,
                result
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                1,
                "successful retry path should flush exactly once"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                failures_before_success + 1,
                "dispatch should wait once initially and once for each transient retry"
            );
            prop_assert_eq!(
                stream.write_attempts.load(Ordering::Relaxed),
                failures_before_success + expected_written.len(),
                "dispatch should retry only the failed frame before continuing in order"
            );

            let mut decoded = Vec::new();
            let mut cursor = Cursor::new(stream.bytes.as_slice());
            while (cursor.position() as usize) < stream.bytes.len() {
                decoded.push(Pdu::decode(&mut cursor).expect("decode recorded outbound PDU"));
            }

            prop_assert_eq!(
                decoded.len(),
                expected_written.len(),
                "transient retry path must not drop or duplicate dispatch frames"
            );
            for (decoded, (expected_serial, expected_pdu)) in
                decoded.iter().zip(expected_written.iter())
            {
                prop_assert_eq!(
                    decoded.serial,
                    *expected_serial,
                    "transient retry path changed outbound serial order"
                );
                prop_assert!(
                    expected_pdu.matches_decoded(&decoded.pdu),
                    "transient retry path changed outbound PDU payload: expected {:?}, got {:?}",
                    expected_pdu,
                    decoded.pdu
                );
            }
            prop_assert_eq!(deferred_item.as_ref().map(classify_item), expected_deferred);

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "transient retry path must preserve queued items after the first deferred non-write",
            );
        }

        #[test]
        fn proptest_write_pending_pdus_classifies_network_write_failures_without_reordering(
            failure in network_write_failures(),
            queued_items in queued_dispatch_items(),
            first_serial in any::<u16>(),
        ) {
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();

            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(test_write_item(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(test_notification_item(MuxNotification::Empty))
                            .expect("queue generated notification");
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                    }
                }
                expected_remaining.push(queued_item);
            }

            let mut deferred_item = None;
            let mut stream = FailingWriteDispatchStream::new(failure);
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(u64::from(first_serial)),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            let err = result.expect_err("network write failure should stop the PDU batch");
            let message = format!("{err:#}");
            prop_assert!(
                message.contains("encoding PDU to client"),
                "write-side network errors should retain dispatch context: {message}"
            );
            prop_assert_eq!(
                is_clean_disconnect(&err),
                failure.is_clean_disconnect(),
                "clean-disconnect classification should match network error kind"
            );
            let expected_write_attempts = if failure.is_transient() {
                TRANSIENT_WRITE_RETRY_LIMIT + 1
            } else {
                1
            };
            prop_assert_eq!(
                stream.write_attempts.load(Ordering::Relaxed),
                expected_write_attempts,
                "dispatch should bound retries for transient failures and stop immediately for hard failures"
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                0,
                "failed writes must not be followed by a flush"
            );
            let expected_writable_waits = if failure.is_transient() {
                TRANSIENT_WRITE_RETRY_LIMIT + 1
            } else {
                1
            };
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                expected_writable_waits,
                "dispatch should wait initially and before each transient retry"
            );
            prop_assert!(
                deferred_item.is_none(),
                "a write failure before queue draining must not manufacture a deferred item"
            );

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "queued dispatch items must remain ordered after a network write failure"
            );
        }

        #[test]
        fn proptest_write_pending_pdus_stops_on_partial_frame_eof_without_dropping_queue(
            failure in partial_frame_failures(),
            queued_items in queued_dispatch_items(),
            first_serial in any::<u16>(),
            cut_seed in any::<usize>(),
        ) {
            let serial = u64::from(first_serial);
            let mut full_frame = Vec::new();
            Pdu::Ping(Ping {})
                .encode(&mut full_frame, serial)
                .expect("generated Ping frame should encode");
            prop_assume!(full_frame.len() > 1);
            let fail_after_bytes = 1 + (cut_seed % (full_frame.len() - 1));

            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();
            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(test_write_item(queued_ping(next_serial)))
                            .expect("queue generated ping");
                        next_serial += 1;
                    }
                    QueuedDispatchItem::Notif => {
                        item_tx
                            .try_send(test_notification_item(MuxNotification::Empty))
                            .expect("queue generated notification");
                    }
                    QueuedDispatchItem::Readable => {
                        item_tx
                            .try_send(Item::Readable)
                            .expect("queue generated readable marker");
                    }
                }
                expected_remaining.push(queued_item);
            }

            let mut deferred_item = None;
            let mut stream = PartialFrameFailingDispatchStream::new(fail_after_bytes, failure);
            let result = promise::spawn::block_on(write_pending_pdus(
                &mut stream,
                queued_ping(serial),
                &item_rx,
                &mut deferred_item,
                None,
            ));

            let err = result.expect_err("partial-frame EOF/write failure should stop dispatch");
            let message = format!("{err:#}");
            prop_assert!(
                message.contains("encoding PDU to client"),
                "partial-frame errors should retain dispatch context: {message}"
            );
            prop_assert_eq!(
                is_clean_disconnect(&err),
                failure.is_clean_disconnect(),
                "partial-frame EOF classification should match the write failure kind"
            );
            prop_assert_eq!(
                stream.bytes.as_slice(),
                &full_frame[..fail_after_bytes],
                "partial write must leave only the strict frame prefix on the stream"
            );
            prop_assert!(
                Pdu::decode(stream.bytes.as_slice()).is_err(),
                "strict partial frame prefix must not decode as a complete PDU"
            );
            prop_assert!(
                stream.write_attempts.load(Ordering::Relaxed) >= 2,
                "write_all should retry after the first short frame write"
            );
            prop_assert_eq!(
                stream.flush_calls.load(Ordering::Relaxed),
                0,
                "partial-frame failures must not be followed by a flush"
            );
            prop_assert_eq!(
                stream.writable_waits.load(Ordering::Relaxed),
                1,
                "dispatch should wait once for writability before attempting the frame"
            );
            prop_assert!(
                deferred_item.is_none(),
                "failure before queue draining must not manufacture a deferred item"
            );

            let mut actual_remaining = Vec::new();
            while let Ok(item) = item_rx.try_recv() {
                actual_remaining.push(classify_item(&item));
            }
            prop_assert_eq!(
                actual_remaining,
                expected_remaining,
                "queued dispatch items must remain ordered after partial-frame EOF"
            );
        }
    }

    #[test]
    fn queue_pdu_reports_full_dispatch_queue() {
        let (item_tx, item_rx) = bounded(1);
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        let err = queue_pdu(&item_tx, Pdu::Ping(Ping {}), 7)
            .expect_err("full queue should produce explicit backpressure error");
        let message = format!("{err:#}");
        assert!(
            message.contains("mux dispatch item queue is full"),
            "queue-full error should be specific: {message}"
        );
        assert!(
            message.contains(&DISPATCH_ITEM_QUEUE_CAPACITY.to_string()),
            "queue-full error should include the configured capacity: {message}"
        );
        assert!(
            matches!(item_rx.try_recv(), Ok(Item::Readable)),
            "failed enqueue must not disturb the queued item"
        );
    }

    #[test]
    fn response_enqueue_failure_trips_connection_terminal() {
        let (item_tx, item_rx) = bounded(1);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        let err = queue_response_pdu(&item_tx, &terminal, Pdu::Pong(Pong {}), 41)
            .expect_err("response enqueue into a full queue must fail");
        assert!(
            format!("{err:#}").contains("mux dispatch item queue is full"),
            "response enqueue should retain the queue failure"
        );
        assert_eq!(
            terminal_rx.try_recv().ok(),
            Some(RESPONSE_QUEUE_FAILURE),
            "a rejected response must terminate the affected connection"
        );
        assert!(
            matches!(item_rx.try_recv(), Ok(Item::Readable)),
            "terminal response failure must not displace the older queued item"
        );
    }

    #[test]
    fn unilateral_conversion_preserves_notification_before_later_response() {
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::WritePdu(Box::new(DecodedPdu {
                pdu: Pdu::Pong(Pong {}),
                serial: 77,
            })))
            .expect("queue later response");
        let mut deferred_item = None;

        let pending = prepare_unilateral_pdu(
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 19 }),
            &item_rx,
            &mut deferred_item,
        )
        .expect("notification and response should encode");

        let mut cursor = Cursor::new(pending.bytes.as_slice());
        let before = Pdu::decode(&mut cursor).expect("decode unilateral frame");
        let after = Pdu::decode(&mut cursor).expect("decode response frame");
        assert!(matches!(
            before,
            DecodedPdu {
                pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 19 }),
                serial: 0,
            }
        ));
        assert!(matches!(
            after,
            DecodedPdu {
                pdu: Pdu::Pong(Pong {}),
                serial: 77,
            }
        ));
        assert_eq!(
            cursor.position() as usize,
            pending.bytes.len(),
            "the batch should contain exactly the notification and response"
        );
        assert!(
            deferred_item.is_none(),
            "the contiguous response should remain in the same ordered batch"
        );
    }

    #[test]
    fn full_notification_queue_terminates_mux_subscriber() {
        let (item_tx, item_rx) = bounded(1);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        assert!(
            !queue_notification(&item_tx, &terminal, MuxNotification::Empty),
            "full notification queues must terminate the subscriber"
        );
        assert!(
            matches!(item_rx.try_recv(), Ok(Item::Readable)),
            "terminal overflow must not displace an older queued item"
        );
        assert_eq!(
            terminal_rx.try_recv().ok(),
            Some(NOTIFICATION_QUEUE_OVERFLOW),
            "notification loss must publish one terminal reason"
        );
    }

    #[test]
    fn write_pending_pdus_batches_flush_and_preserves_first_non_write_item() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .expect("global test lock");
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::WritePdu(queued_ping(2)))
            .expect("queue second ping");
        item_tx
            .try_send(Item::Notif(MuxNotification::Empty))
            .expect("queue notification");

        let mut deferred_item = None;
        let mut stream = CountingDispatchStream::default();
        let result = promise::spawn::block_on(write_pending_pdus(
            &mut stream,
            queued_ping(1),
            &item_rx,
            &mut deferred_item,
            None,
        ));

        assert!(result.is_ok(), "batched write helper should succeed");
        assert!(
            stream.bytes_written.load(Ordering::Relaxed) > 0,
            "encoded PDUs should write bytes"
        );
        assert_eq!(
            stream.flush_calls.load(Ordering::Relaxed),
            1,
            "batched writes should flush once"
        );
        assert_eq!(
            stream.writable_waits.load(Ordering::Relaxed),
            1,
            "batched writes should wait once"
        );
        assert!(
            matches!(deferred_item, Some(Item::Notif(MuxNotification::Empty))),
            "first non-write item should be preserved for the main loop"
        );
        assert!(
            matches!(item_rx.try_recv(), Err(TryRecvError::Empty)),
            "write batch should drain only queued write items"
        );
    }

    #[test]
    fn dispatch_chooser_services_readable_input_before_a_continuous_internal_queue() {
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::Notif(MuxNotification::Empty))
            .expect("queue outbound notification");
        let stream = RecordingDispatchStream::default();
        let mut deferred_item = None;
        let mut prefer_read = true;

        let first = promise::spawn::block_on(next_dispatch_item(
            &stream,
            &item_rx,
            &mut deferred_item,
            None,
            &mut prefer_read,
        ))
        .expect("ready input should be selected");
        assert!(
            matches!(first, Item::Readable),
            "an already-readable keypress must not wait for the internal queue to become empty",
        );
        assert!(!prefer_read);

        let second = promise::spawn::block_on(next_dispatch_item(
            &stream,
            &item_rx,
            &mut deferred_item,
            None,
            &mut prefer_read,
        ))
        .expect("outbound turn should remain live");
        assert!(matches!(second, Item::Notif(MuxNotification::Empty)));
        assert!(
            prefer_read,
            "the next turn must force another inbound readiness probe",
        );
    }

    #[test]
    fn oversized_outbound_frame_yields_between_bounded_chunks_for_inbound_work() {
        let (item_tx, item_rx) = unbounded();
        let first = Box::new(DecodedPdu {
            pdu: Pdu::WriteToPane(WriteToPane {
                pane_id: 7,
                data: vec![0x5a; OUTBOUND_WRITE_QUANTUM_BYTES * 3],
            }),
            serial: 1,
        });
        let mut deferred_item = None;
        let mut pending = prepare_pending_outbound_batch(
            first,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
        )
        .expect("large frame should encode");
        drop(item_tx);

        assert!(
            pending.bytes.len() > OUTBOUND_WRITE_QUANTUM_BYTES,
            "test frame must exceed one outbound service quantum",
        );
        let mut stream = ChunkedDispatchStream::default();
        let first_turn =
            promise::spawn::block_on(service_pending_outbound(&mut stream, &mut pending, None))
                .expect("first write chunk should succeed");
        assert_eq!(first_turn, OutboundService::Progress);
        assert_eq!(pending.offset, OUTBOUND_WRITE_QUANTUM_BYTES);
        assert_eq!(stream.write_sizes, vec![OUTBOUND_WRITE_QUANTUM_BYTES]);

        stream.readable.store(true, Ordering::Release);
        let offset_before_read = pending.offset;
        let inbound_turn =
            promise::spawn::block_on(service_pending_outbound(&mut stream, &mut pending, None))
                .expect("readable input should preempt the next frame chunk");
        assert_eq!(inbound_turn, OutboundService::Readable);
        assert_eq!(
            pending.offset, offset_before_read,
            "inbound service must not advance the outbound frame",
        );

        stream.readable.store(false, Ordering::Release);
        loop {
            let outcome =
                promise::spawn::block_on(service_pending_outbound(&mut stream, &mut pending, None))
                    .expect("remaining chunks should succeed");
            assert!(matches!(
                outcome,
                OutboundService::Progress | OutboundService::Complete
            ));
            if outcome == OutboundService::Complete {
                break;
            }
        }
        assert!(
            stream
                .write_sizes
                .iter()
                .all(|written| *written <= OUTBOUND_WRITE_QUANTUM_BYTES),
            "no write turn may exceed the configured byte quantum",
        );
        assert_eq!(stream.bytes, pending.bytes);
    }

    #[test]
    fn pending_write_rearms_combined_interest_and_yields_to_new_input() {
        let (_item_tx, item_rx) = unbounded();
        let first = Box::new(DecodedPdu {
            pdu: Pdu::Ping(Ping {}),
            serial: 1,
        });
        let mut deferred_item = None;
        let mut pending = prepare_pending_outbound_batch(
            first,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
        )
        .expect("ping frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream::default();

        let outcome =
            promise::spawn::block_on(service_pending_outbound(&mut stream, &mut pending, None))
                .expect("newly readable input should preempt a pending write");

        assert_eq!(outcome, OutboundService::Readable);
        assert_eq!(pending.offset, 0, "a Pending write must not consume bytes");
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unsupported_readiness_probe_classifies_combined_read_wake_without_spinning() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let mut pending = prepare_pending_outbound_batch(
            queued_ping(1),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
        )
        .expect("ping frame should encode");
        let mut stream = UnsupportedReadinessPendingWriteStream::default();

        for expected_polls in 1..=2 {
            let outcome =
                promise::spawn::block_on(service_pending_outbound(&mut stream, &mut pending, None))
                    .expect("classified read wake should return to the dispatch loop");

            assert_eq!(outcome, OutboundService::Readable);
            assert_eq!(
                pending.offset, 0,
                "a Pending write must not consume outbound bytes",
            );
            assert_eq!(
                stream.write_polls.load(Ordering::Relaxed),
                expected_polls,
                "each reciprocal turn must poll outbound once before yielding to input",
            );
            assert_eq!(
                stream.readable_waits.load(Ordering::Relaxed),
                expected_polls,
                "one classified combined wait should resolve each service turn",
            );
        }
    }

    #[test]
    fn prefetched_bytes_are_reported_readable_before_the_inner_socket() {
        let stream = PrefetchedDispatchStream::new(CountingDispatchStream::default(), vec![0x42]);
        assert_eq!(
            stream
                .try_readable_without_consuming()
                .expect("prefetched readiness probe should succeed"),
            DispatchReadinessHint::Ready,
        );
    }

    #[test]
    fn outbound_write_batch_yields_at_the_frame_quantum() {
        let (item_tx, item_rx) = unbounded();
        let total_frames = OUTBOUND_WRITE_QUANTUM_FRAMES + 5;
        for serial in 2..=u64::try_from(total_frames).expect("test frame count fits u64") {
            item_tx
                .try_send(Item::WritePdu(queued_ping(serial)))
                .expect("queue outbound ping");
        }
        let mut deferred_item = None;
        let pending = prepare_pending_outbound_batch(
            queued_ping(1),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Auto,
        )
        .expect("bounded outbound batch should prepare");

        let mut decoded_frames = 0;
        let mut cursor = Cursor::new(pending.bytes.as_slice());
        while (cursor.position() as usize) < pending.bytes.len() {
            Pdu::decode(&mut cursor).expect("decode recorded outbound frame");
            decoded_frames += 1;
        }
        assert_eq!(decoded_frames, OUTBOUND_WRITE_QUANTUM_FRAMES);
        assert!(deferred_item.is_none());
        assert_eq!(
            item_rx.len(),
            total_frames - OUTBOUND_WRITE_QUANTUM_FRAMES,
            "the next turn must retain the ordered outbound suffix",
        );
    }

    #[test]
    fn dispatch_backend_reports_platform_default() {
        let backend = DispatchIoBackend::current_default();

        #[cfg(all(feature = "io-uring", target_os = "linux"))]
        assert_eq!(backend, DispatchIoBackend::IoUring);
        #[cfg(all(not(feature = "io-uring"), target_os = "linux"))]
        assert_eq!(backend, DispatchIoBackend::Epoll);
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        assert_eq!(backend, DispatchIoBackend::Kqueue);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        assert_eq!(backend, DispatchIoBackend::Poll);
    }

    #[test]
    fn auto_prefers_io_uring_for_unix_when_available() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Auto),
            DispatchStreamKind::Unix,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: true,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::IoUring);
        assert_eq!(reactor.fallback_reason(), None);
    }

    #[test]
    fn auto_falls_back_when_io_uring_unavailable() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Auto),
            DispatchStreamKind::Unix,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: false,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::readiness_default());
        assert!(reactor.fallback_reason().is_some());
    }

    #[test]
    fn tls_forces_readiness_even_when_io_uring_is_available() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::IoUring),
            DispatchStreamKind::Tls,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: true,
                io_uring_runtime_available: true,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::readiness_default());
        assert!(
            reactor
                .fallback_reason()
                .is_some_and(|reason| reason.contains("UnixStream"))
        );
    }

    #[test]
    fn explicit_poll_stays_poll() {
        let reactor = DispatchReactor::resolve_with_availability(
            DispatchRuntimeConfig::new(DispatchIoPreference::Poll),
            DispatchStreamKind::Generic,
            DispatchIoRuntimeAvailability {
                io_uring_compiled: false,
                io_uring_runtime_available: false,
            },
        );

        assert_eq!(reactor.backend(), DispatchIoBackend::Poll);
        assert_eq!(reactor.fallback_reason(), None);
    }

    #[cfg(all(feature = "io-uring", target_os = "linux"))]
    #[test]
    fn io_uring_runtime_waits_for_unix_fd_readability() {
        let runtime = match DispatchIoUringRuntime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                eprintln!("skipping io_uring readability test: {err}");
                return;
            }
        };
        let (left, mut right) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        left.set_nonblocking(true).expect("left nonblocking");
        right.set_nonblocking(true).expect("right nonblocking");

        let writer = std::thread::spawn(move || {
            right.write_all(b"ping").expect("write ping");
        });

        let result =
            promise::spawn::block_on(runtime.wait_for_fd(left.as_raw_fd(), Interest::READABLE));
        writer.join().expect("writer thread");
        assert!(
            result.is_ok(),
            "readability wait should succeed: {result:?}"
        );
    }
}
