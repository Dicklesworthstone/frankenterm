#![allow(clippy::future_not_send)]
#![allow(clippy::type_repetition_in_bounds)]
use crate::sessionhandler::{
    AdmittedInputTraceV1, DispatchTraceAuthority, PduDeliveryClass, PduSender, PerPane,
    SessionAuthority, SessionHandler, SessionOwner, SessionTraceProducer,
    frozen_window_order_to_codec, retire_poisoned_pane_render,
    validate_ordered_snapshot_projection,
};
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
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
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
    "mux outbound delivery exceeded its retained-memory or slot bound";
const TOPOLOGY_REVISION_EXHAUSTED: &str = "mux topology revision authority is exhausted";
const DORMANT_OUTBOUND_PROTOCOL_FAILURE: &str =
    "mux server attempted to emit a protocol family without live activation authority";
const OUTBOUND_WIRE_AUTHORITY_FAILURE: &str =
    "mux server attempted to emit a protocol family with the wrong wire role";
const PANE_ALERT_BACKLOG_FAILURE: &str =
    "mux pane alert backlog could not retain a delivery obligation";
const TOPOLOGY_FENCE_MAX_EVENTS: usize = 4096;
const TOPOLOGY_FENCE_MAX_RETAINED_BYTES: usize = 4 * 1024 * 1024;
/// Accounted topology values retain their original 4 MiB connection ceiling;
/// a complete snapshot frame has a separate ceiling shared by PDU82 and PDU87.
/// Their aggregate
/// permits one maximum snapshot to coexist with the separately bounded
/// successor queue, while each class remains unable to borrow the other's
/// tranche. This retained-owner budget begins when the complete encoded frame
/// is reserved; codec-bounded transient serialization/compression workspace is
/// a separate peak-memory concern and is not represented by these counters.
const OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES: usize =
    codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES + TOPOLOGY_FENCE_MAX_RETAINED_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TopologyRetentionLimits {
    max_events: usize,
    max_retained_bytes: usize,
}

impl Default for TopologyRetentionLimits {
    fn default() -> Self {
        Self {
            max_events: TOPOLOGY_FENCE_MAX_EVENTS,
            max_retained_bytes: TOPOLOGY_FENCE_MAX_RETAINED_BYTES,
        }
    }
}

/// Whether this server-produced family is deliberately dormant on the
/// ordinary mux transport.
///
/// PDU 76 and 78 are replies to same-family client requests, while PDU 82 and
/// the only live post-v46 unilateral family (PDU 83) are fenced by the PDU 81
/// topology negotiation.  Those request-implied authorities do not require
/// the server to guess the client's otherwise-unobservable codec window.
///
/// In contrast, render-application and exact-render delivery have no live
/// ordinary-server coordinator, and ordered-window support is intentionally
/// absent from `TopologyCapabilities::SERVER_SUPPORTED`. Keep every
/// server-produced PDU in those dormant families fail-closed until its own
/// activation work lands.
fn is_dormant_server_wire_spec(spec: &codec::PduWireSpec) -> bool {
    let ident = spec.ident;
    ident == <codec::RenderApplicationUpdateV1 as codec::PduWireIdent>::IDENT
        || ident == <codec::RenderApplicationUpdate as codec::PduWireIdent>::IDENT
        || ident == <codec::ListPanesOrderedV1Response as codec::PduWireIdent>::IDENT
        || ident == <codec::ReorderWindowTabsV1Response as codec::PduWireIdent>::IDENT
        || ident == <codec::WindowOrderEventV1 as codec::PduWireIdent>::IDENT
        || ident == <codec::GetPaneRenderDeliveryV1Response as codec::PduWireIdent>::IDENT
}

/// Proof carried with a typed outbound PDU from its family-specific dispatch
/// coordinator to the final encoder chokepoint.
///
/// Ordered-window schemas remain dormant in `SERVER_SUPPORTED`; these permits
/// are therefore constructible only by the future-enabled ordered fence below.
/// Keeping the proof on the queued value prevents a generic response path from
/// bypassing dormancy merely because the codec knows the PDU shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerEmissionAuthority {
    Ordinary,
    OrderedSnapshotFence,
    OrderedStreamEvent,
}

impl ServerEmissionAuthority {
    #[cfg(test)]
    fn permits(self, pdu: &Pdu, serial: u64) -> bool {
        pdu.wire_spec()
            .is_some_and(|spec| self.permits_wire(spec, serial))
    }

    fn permits_wire(self, spec: &codec::PduWireSpec, serial: u64) -> bool {
        (self == Self::OrderedSnapshotFence
            && spec.ident == <codec::ListPanesOrderedV1Response as codec::PduWireIdent>::IDENT
            && serial != 0)
            || (self == Self::OrderedStreamEvent
                && spec.ident == <codec::WindowOrderEventV1 as codec::PduWireIdent>::IDENT
                && serial == 0)
    }
}

/// Immutable identity retained when a typed PDU becomes a byte frame. This
/// prevents deferred/pre-encoded frames from bypassing the same dormant-family
/// guard that applies to the typed queue path.
#[derive(Clone, Copy, Debug)]
struct EncodedPduAuthority {
    wire_spec: Option<&'static codec::PduWireSpec>,
    serial: u64,
    emission: ServerEmissionAuthority,
}

impl EncodedPduAuthority {
    fn capture(pdu: &Pdu, serial: u64, emission: ServerEmissionAuthority) -> Self {
        Self {
            wire_spec: pdu.wire_spec(),
            serial,
            emission,
        }
    }

    fn capture_wire(
        wire_spec: &'static codec::PduWireSpec,
        serial: u64,
        emission: ServerEmissionAuthority,
    ) -> Self {
        Self {
            wire_spec: Some(wire_spec),
            serial,
            emission,
        }
    }

    fn validate(self, terminal: &DispatchTerminal) -> anyhow::Result<()> {
        validate_server_wire_emission_authority(
            self.wire_spec,
            self.serial,
            terminal,
            self.emission,
        )
    }
}

/// Final connection-terminal guard against accidentally activating a frozen
/// server-produced protocol family through a generic response or notification
/// queue. Callers invoke this before queue allocation; typed-to-encoded and
/// already-encoded paths retain and recheck the same immutable authority.
fn validate_server_emission_authority(
    pdu: &Pdu,
    serial: u64,
    terminal: &DispatchTerminal,
    authority: ServerEmissionAuthority,
) -> anyhow::Result<()> {
    validate_server_wire_emission_authority(pdu.wire_spec(), serial, terminal, authority)
}

fn validate_server_wire_emission_authority(
    wire_spec: Option<&'static codec::PduWireSpec>,
    serial: u64,
    terminal: &DispatchTerminal,
    authority: ServerEmissionAuthority,
) -> anyhow::Result<()> {
    let Some(spec) = wire_spec else {
        metrics::counter!(
            "mux.dispatch.protocol_error",
            "reason" => "unassigned_server_wire_family",
            "pdu" => "unassigned",
        )
        .increment(1);
        terminal.trip(OUTBOUND_WIRE_AUTHORITY_FAILURE);
        anyhow::bail!("mux server attempted to emit an unassigned PDU family (serial {serial})");
    };
    let role = if serial == 0 {
        codec::PduWireRole::Unilateral
    } else {
        codec::PduWireRole::CorrelatedReply
    };
    if !spec.authorizes(codec::PduProducer::Server, role) {
        metrics::counter!(
            "mux.dispatch.protocol_error",
            "reason" => "server_wire_authority",
            "pdu" => spec.name,
        )
        .increment(1);
        terminal.trip(OUTBOUND_WIRE_AUTHORITY_FAILURE);
        anyhow::bail!(
            "mux server PDU {} (ident {}, serial {}) cannot emit as {:?}",
            spec.name,
            spec.ident,
            serial,
            role,
        );
    }
    if !is_dormant_server_wire_spec(spec) || authority.permits_wire(spec, serial) {
        return Ok(());
    }

    metrics::counter!(
        "mux.dispatch.protocol_error",
        "reason" => "dormant_server_emission",
        "pdu" => spec.name,
    )
    .increment(1);
    terminal.trip(DORMANT_OUTBOUND_PROTOCOL_FAILURE);
    anyhow::bail!(
        "mux server PDU {} (ident {}, serial {}) has no live family-specific emission authority",
        spec.name,
        spec.ident,
        serial,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundClass {
    Control,
    Bulk,
    Topology,
    Snapshot,
}

impl OutboundClass {
    const fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Bulk => "bulk",
            Self::Topology => "topology",
            Self::Snapshot => "snapshot",
        }
    }

    const fn is_bulk(self) -> bool {
        matches!(self, Self::Bulk | Self::Topology)
    }

    const fn batch_class(self) -> OutboundBatchClass {
        match self {
            Self::Control | Self::Bulk => OutboundBatchClass::Control,
            Self::Topology => OutboundBatchClass::Topology,
            Self::Snapshot => OutboundBatchClass::Snapshot,
        }
    }

    const fn retained_class(self) -> OutboundRetainedClass {
        match self {
            Self::Control | Self::Bulk => OutboundRetainedClass::None,
            Self::Topology => OutboundRetainedClass::Topology,
            Self::Snapshot => OutboundRetainedClass::Snapshot,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundBatchClass {
    Control,
    Topology,
    Snapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundRetainedClass {
    None,
    Topology,
    Snapshot,
}

impl OutboundRetainedClass {
    const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Topology => "topology",
            Self::Snapshot => "snapshot",
        }
    }

    const fn maximum(self) -> usize {
        match self {
            Self::None => 0,
            Self::Topology => TOPOLOGY_FENCE_MAX_RETAINED_BYTES,
            Self::Snapshot => codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundBudgetLimit {
    Arithmetic,
    TotalSlots,
    BulkSlots,
    RetainedBytes,
    TopologyRetainedBytes,
    SnapshotRetainedBytes,
    MixedConnection,
    MixedRetainedClass,
}

impl OutboundBudgetLimit {
    const fn label(self) -> &'static str {
        match self {
            Self::Arithmetic => "arithmetic",
            Self::TotalSlots => "total_slots",
            Self::BulkSlots => "bulk_slots",
            Self::RetainedBytes => "retained_bytes",
            Self::TopologyRetainedBytes => "topology_retained_bytes",
            Self::SnapshotRetainedBytes => "snapshot_retained_bytes",
            Self::MixedConnection => "mixed_connection",
            Self::MixedRetainedClass => "mixed_retained_class",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OutboundBudgetState {
    retained_bytes: usize,
    topology_retained_bytes: usize,
    snapshot_retained_bytes: usize,
    total_slots: usize,
    bulk_slots: usize,
    peak_retained_bytes: usize,
}

#[derive(Debug, Default)]
struct OutboundBudget {
    state: ParkingMutex<OutboundBudgetState>,
}

impl OutboundBudget {
    fn checked_reservation_state(
        mut state: OutboundBudgetState,
        class: OutboundClass,
        retained_bytes: usize,
    ) -> Result<OutboundBudgetState, OutboundBudgetLimit> {
        if class.retained_class() == OutboundRetainedClass::None && retained_bytes != 0 {
            return Err(OutboundBudgetLimit::Arithmetic);
        }
        state.total_slots = state
            .total_slots
            .checked_add(1)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if state.total_slots > DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY {
            return Err(OutboundBudgetLimit::TotalSlots);
        }

        state.bulk_slots = state
            .bulk_slots
            .checked_add(usize::from(class.is_bulk()))
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if state.bulk_slots > DISPATCH_ITEM_QUEUE_CAPACITY {
            return Err(OutboundBudgetLimit::BulkSlots);
        }

        state.retained_bytes = state
            .retained_bytes
            .checked_add(retained_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if state.retained_bytes > OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES {
            return Err(OutboundBudgetLimit::RetainedBytes);
        }

        let class_bytes = match class.retained_class() {
            OutboundRetainedClass::None => 0,
            OutboundRetainedClass::Topology => {
                state.topology_retained_bytes = state
                    .topology_retained_bytes
                    .checked_add(retained_bytes)
                    .ok_or(OutboundBudgetLimit::Arithmetic)?;
                state.topology_retained_bytes
            }
            OutboundRetainedClass::Snapshot => {
                state.snapshot_retained_bytes = state
                    .snapshot_retained_bytes
                    .checked_add(retained_bytes)
                    .ok_or(OutboundBudgetLimit::Arithmetic)?;
                state.snapshot_retained_bytes
            }
        };
        if class_bytes > class.retained_class().maximum() {
            return Err(match class.retained_class() {
                OutboundRetainedClass::None => OutboundBudgetLimit::Arithmetic,
                OutboundRetainedClass::Topology => OutboundBudgetLimit::TopologyRetainedBytes,
                OutboundRetainedClass::Snapshot => OutboundBudgetLimit::SnapshotRetainedBytes,
            });
        }
        state.peak_retained_bytes = state.peak_retained_bytes.max(state.retained_bytes);
        Ok(state)
    }

    fn preflight(
        &self,
        class: OutboundClass,
        minimum_retained_bytes: usize,
    ) -> Result<(), OutboundBudgetLimit> {
        Self::checked_reservation_state(*self.state.lock(), class, minimum_retained_bytes)
            .map(|_| ())
    }

    fn try_reserve(
        self: &Arc<Self>,
        class: OutboundClass,
        retained_bytes: usize,
    ) -> Result<OutboundReservation, OutboundBudgetLimit> {
        let mut state = self.state.lock();
        *state = Self::checked_reservation_state(*state, class, retained_bytes)?;
        Ok(OutboundReservation {
            budget: Arc::clone(self),
            retained_bytes,
            total_slots: 1,
            bulk_slots: usize::from(class.is_bulk()),
            batch_class: class.batch_class(),
            retained_class: class.retained_class(),
        })
    }

    fn reweight_accounted_batch(
        reservations: &mut [OutboundReservation],
        retained_bytes: usize,
    ) -> Result<(), OutboundBudgetLimit> {
        let Some(first_accounted_index) = reservations
            .iter()
            .position(OutboundReservation::accounts_retained_bytes)
        else {
            return Ok(());
        };
        let budget = Arc::clone(&reservations[first_accounted_index].budget);
        let retained_class = reservations[first_accounted_index].retained_class;
        if reservations
            .iter()
            .any(|reservation| !Arc::ptr_eq(&budget, &reservation.budget))
        {
            return Err(OutboundBudgetLimit::MixedConnection);
        }
        if reservations.iter().any(|reservation| {
            reservation.accounts_retained_bytes() && reservation.retained_class != retained_class
        }) {
            return Err(OutboundBudgetLimit::MixedRetainedClass);
        }
        let current_batch_bytes = reservations
            .iter()
            .filter(|reservation| reservation.accounts_retained_bytes())
            .try_fold(0_usize, |total, reservation| {
                total.checked_add(reservation.retained_bytes)
            })
            .ok_or(OutboundBudgetLimit::Arithmetic)?;

        let mut state = budget.state.lock();
        let other_retained_bytes = state
            .retained_bytes
            .checked_sub(current_batch_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        let next_retained_bytes = other_retained_bytes
            .checked_add(retained_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_retained_bytes > OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES {
            return Err(OutboundBudgetLimit::RetainedBytes);
        }
        let current_class_total = match retained_class {
            OutboundRetainedClass::None => return Err(OutboundBudgetLimit::Arithmetic),
            OutboundRetainedClass::Topology => state.topology_retained_bytes,
            OutboundRetainedClass::Snapshot => state.snapshot_retained_bytes,
        };
        let other_class_bytes = current_class_total
            .checked_sub(current_batch_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        let next_class_bytes = other_class_bytes
            .checked_add(retained_bytes)
            .ok_or(OutboundBudgetLimit::Arithmetic)?;
        if next_class_bytes > retained_class.maximum() {
            return Err(match retained_class {
                OutboundRetainedClass::None => OutboundBudgetLimit::Arithmetic,
                OutboundRetainedClass::Topology => OutboundBudgetLimit::TopologyRetainedBytes,
                OutboundRetainedClass::Snapshot => OutboundBudgetLimit::SnapshotRetainedBytes,
            });
        }
        match retained_class {
            OutboundRetainedClass::None => unreachable!("accounted batch has a retained class"),
            OutboundRetainedClass::Topology => {
                state.topology_retained_bytes = next_class_bytes;
            }
            OutboundRetainedClass::Snapshot => {
                state.snapshot_retained_bytes = next_class_bytes;
            }
        }
        state.retained_bytes = next_retained_bytes;
        state.peak_retained_bytes = state.peak_retained_bytes.max(next_retained_bytes);
        for (index, reservation) in reservations.iter_mut().enumerate() {
            if reservation.accounts_retained_bytes() {
                reservation.retained_bytes = if index == first_accounted_index {
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
    retained_bytes: usize,
    total_slots: usize,
    bulk_slots: usize,
    batch_class: OutboundBatchClass,
    retained_class: OutboundRetainedClass,
}

impl OutboundReservation {
    const fn accounts_retained_bytes(&self) -> bool {
        !matches!(self.retained_class, OutboundRetainedClass::None)
    }
}

impl Drop for OutboundReservation {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock();
        state.retained_bytes = state
            .retained_bytes
            .checked_sub(self.retained_bytes)
            .expect("outbound retained-byte reservation underflow");
        match self.retained_class {
            OutboundRetainedClass::None => {}
            OutboundRetainedClass::Topology => {
                state.topology_retained_bytes = state
                    .topology_retained_bytes
                    .checked_sub(self.retained_bytes)
                    .expect("outbound topology reservation underflow");
            }
            OutboundRetainedClass::Snapshot => {
                state.snapshot_retained_bytes = state
                    .snapshot_retained_bytes
                    .checked_sub(self.retained_bytes)
                    .expect("outbound snapshot reservation underflow");
            }
        }
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

#[derive(Clone, Debug)]
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
    retained_bytes: usize,
) -> anyhow::Result<OutboundReservation> {
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    match reserve_outbound_admitted(budget, class, retained_bytes) {
        Ok(reservation) => Ok(reservation),
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            Err(outbound_budget_rejection(class, limit))
        }
    }
}

fn reserve_outbound_admitted(
    budget: &Arc<OutboundBudget>,
    class: OutboundClass,
    retained_bytes: usize,
) -> Result<OutboundReservation, OutboundBudgetLimit> {
    budget.try_reserve(class, retained_bytes)
}

fn outbound_budget_rejection(class: OutboundClass, limit: OutboundBudgetLimit) -> anyhow::Error {
    metrics::counter!(
        "mux.dispatch.outbound_budget.rejected",
        "class" => class.label(),
        "limit" => limit.label(),
    )
    .increment(1);
    anyhow::anyhow!(
        "mux outbound {class:?} reservation exceeded the {} bound",
        limit.label()
    )
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

#[derive(Clone, Debug)]
pub struct DispatchRuntimeConfig {
    preference: DispatchIoPreference,
    trace_authority: Option<Arc<DispatchTraceAuthority>>,
}

impl DispatchRuntimeConfig {
    #[must_use]
    pub const fn new(preference: DispatchIoPreference) -> Self {
        Self {
            preference,
            trace_authority: None,
        }
    }

    #[must_use]
    pub const fn preference(&self) -> DispatchIoPreference {
        self.preference
    }

    /// Bind one explicit recorder authority to subsequently accepted binary
    /// mux connections. Producer registration occurs once per connection,
    /// before request decoding, and never through a global singleton.
    #[must_use]
    pub fn with_trace_authority(mut self, authority: Arc<DispatchTraceAuthority>) -> Self {
        self.trace_authority = Some(authority);
        self
    }

    fn trace_authority(&self) -> Option<Arc<DispatchTraceAuthority>> {
        self.trace_authority.as_ref().map(Arc::clone)
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
    emission_authority: ServerEmissionAuthority,
}

#[derive(Debug)]
struct AuthorizedOrderedSnapshotFrame {
    bytes: Vec<u8>,
    authority: EncodedPduAuthority,
}

impl AuthorizedOrderedSnapshotFrame {
    fn encode(
        response: codec::ValidatedListPanesOrderedV1Response,
        serial: u64,
        terminal: &DispatchTerminal,
    ) -> anyhow::Result<Self> {
        let authority = EncodedPduAuthority::capture_wire(
            &<codec::ListPanesOrderedV1Response as codec::PduWireIdent>::WIRE_SPEC,
            serial,
            ServerEmissionAuthority::OrderedSnapshotFence,
        );
        authority.validate(terminal)?;
        let encoded = response.encode_frame(serial);
        if let Err(error) = &encoded {
            record_snapshot_body_limit_mismatch("pdu87", error);
        }
        let bytes =
            encoded.context("encoding request-correlated PDU87 outside the coordinator lock")?;
        Ok(Self { bytes, authority })
    }

    fn reserve(
        self,
        terminal: &DispatchTerminal,
        budget: &Arc<OutboundBudget>,
    ) -> anyhow::Result<EncodedOutboundFrame> {
        let retained_bytes = self.bytes.capacity();
        let reservation =
            reserve_outbound(terminal, budget, OutboundClass::Snapshot, retained_bytes)?;
        Ok(EncodedOutboundFrame {
            bytes: self.bytes,
            reservation,
            authority: self.authority,
        })
    }
}

fn record_snapshot_body_limit_mismatch(family: &'static str, error: &anyhow::Error) {
    if error
        .downcast_ref::<codec::PduEncodedBodyLimitExceeded>()
        .is_some()
    {
        metrics::counter!(
            "mux.server.pane_snapshot_metadata.body_limit_mismatch_total",
            "family" => family,
        )
        .increment(1);
    }
}

#[derive(Debug)]
struct EncodedOutboundFrame {
    bytes: Vec<u8>,
    reservation: OutboundReservation,
    authority: EncodedPduAuthority,
}

#[derive(Debug)]
enum WritePayload {
    Typed(ReservedDecodedPdu),
    Encoded(EncodedOutboundFrame),
}

impl WritePayload {
    const fn batch_class(&self) -> OutboundBatchClass {
        match self {
            Self::Typed(typed) => typed.reservation.batch_class,
            Self::Encoded(frame) => frame.reservation.batch_class,
        }
    }

    const fn accounts_retained_bytes(&self) -> bool {
        match self {
            Self::Typed(typed) => typed.reservation.accounts_retained_bytes(),
            Self::Encoded(frame) => frame.reservation.accounts_retained_bytes(),
        }
    }
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

#[cfg(test)]
fn pdu_item(pdu: Pdu, serial: u64, reservation: OutboundReservation) -> Item {
    decoded_pdu_item(Box::new(DecodedPdu { pdu, serial }), reservation)
}

#[cfg(test)]
fn decoded_pdu_item(decoded: Box<DecodedPdu>, reservation: OutboundReservation) -> Item {
    decoded_pdu_item_with_emission_authority(
        decoded,
        reservation,
        ServerEmissionAuthority::Ordinary,
    )
}

fn decoded_pdu_item_with_emission_authority(
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
    emission_authority: ServerEmissionAuthority,
) -> Item {
    Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu {
        decoded,
        reservation,
        emission_authority,
    }))
}

#[cfg(test)]
fn test_reservation(class: OutboundClass) -> OutboundReservation {
    let budget = Arc::new(OutboundBudget::default());
    budget
        .try_reserve(class, 0)
        .expect("an empty test outbound budget should admit one item")
}

#[cfg(test)]
fn test_write_payload(decoded: Box<DecodedPdu>) -> WritePayload {
    let reservation = test_reservation(OutboundClass::Control);
    WritePayload::Typed(ReservedDecodedPdu {
        decoded,
        reservation,
        emission_authority: ServerEmissionAuthority::Ordinary,
    })
}

#[cfg(test)]
fn test_write_item(decoded: Box<DecodedPdu>) -> Item {
    Item::WritePdu(test_write_payload(decoded))
}

#[cfg(test)]
fn test_encoded_authority(serial: u64) -> EncodedPduAuthority {
    EncodedPduAuthority::capture(
        &Pdu::Pong(codec::Pong {}),
        serial,
        ServerEmissionAuthority::Ordinary,
    )
}

#[cfg(test)]
fn test_notification_item(notification: MuxNotification) -> Item {
    let reservation = test_reservation(OutboundClass::Bulk);
    Item::Notif(ReservedNotification {
        notification,
        reservation,
    })
}

#[cfg(test)]
fn test_terminal() -> DispatchTerminal {
    DispatchTerminal::channel().0
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
    delivery_class: PduDeliveryClass,
) -> anyhow::Result<()> {
    queue_response_pdu_with_emission_authority(
        item_tx,
        terminal,
        budget,
        pdu,
        serial,
        delivery_class,
        ServerEmissionAuthority::Ordinary,
    )
}

fn queue_response_pdu_with_emission_authority(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    pdu: Pdu,
    serial: u64,
    delivery_class: PduDeliveryClass,
    emission_authority: ServerEmissionAuthority,
) -> anyhow::Result<()> {
    let class = match delivery_class {
        PduDeliveryClass::Control => OutboundClass::Control,
        PduDeliveryClass::Bulk => OutboundClass::Bulk,
    };
    queue_response_pdu_with_accounting(
        item_tx,
        terminal,
        budget,
        pdu,
        serial,
        class,
        0,
        emission_authority,
    )
}

fn queue_response_pdu_with_accounting(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    pdu: Pdu,
    serial: u64,
    class: OutboundClass,
    retained_bytes: usize,
    emission_authority: ServerEmissionAuthority,
) -> anyhow::Result<()> {
    validate_server_emission_authority(&pdu, serial, terminal, emission_authority)?;
    // Avoid even the outbound Box allocation on an already-dead connection;
    // `admit` below remains the authoritative race-closing check.
    if terminal.is_tripped() {
        anyhow::bail!("mux dispatch connection is already terminal");
    }
    // Box the response before entering the connection's short admission
    // section. Large coherent snapshots must not extend this critical section
    // with allocator work. Admission, budget reservation, and FIFO publication
    // form one linearization section so concurrent producers cannot consume
    // headroom in one order and publish in another.
    let decoded = Box::new(DecodedPdu { pdu, serial });
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    let reservation = match reserve_outbound_admitted(budget, class, retained_bytes) {
        Ok(reservation) => reservation,
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            return Err(outbound_budget_rejection(class, limit));
        }
    };
    let item = decoded_pdu_item_with_emission_authority(decoded, reservation, emission_authority);
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(item)) => {
            admission.trip(RESPONSE_QUEUE_FAILURE);
            drop(admission);
            drop(item);
            metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
            Err(anyhow::anyhow!(
                "mux dispatch item queue is full (capacity \
                 {DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY}); applying client backpressure"
            ))
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(RESPONSE_QUEUE_FAILURE);
            drop(admission);
            drop(item);
            metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
            Err(anyhow::anyhow!("mux dispatch item queue is closed"))
        }
    }
}

fn pane_entry_metadata_retained_bytes(entry: &mux::tab::PaneEntry) -> anyhow::Result<usize> {
    [
        entry.title.capacity(),
        entry
            .working_dir
            .as_ref()
            .map_or(0, mux::tab::SerdeUrl::capacity),
        entry.workspace.capacity(),
        entry.tty_name.as_ref().map_or(0, String::capacity),
    ]
    .iter()
    .try_fold(0usize, |total, bytes| {
        total
            .checked_add(*bytes)
            .ok_or_else(|| anyhow::anyhow!("coherent snapshot metadata byte accounting overflow"))
    })
}

fn pane_node_metadata_retained_bytes(
    node: &mux::tab::PaneNode,
    depth: usize,
) -> anyhow::Result<usize> {
    if depth > codec::MAX_ORDERED_PANE_TREE_DEPTH {
        anyhow::bail!("coherent snapshot metadata tree exceeds its depth bound");
    }
    match node {
        mux::tab::PaneNode::Empty => Ok(0),
        mux::tab::PaneNode::Leaf(entry) => pane_entry_metadata_retained_bytes(entry),
        mux::tab::PaneNode::Split { left, right, .. } => {
            let next_depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("coherent snapshot metadata tree depth overflow"))?;
            pane_node_metadata_retained_bytes(left, next_depth)?
                .checked_add(pane_node_metadata_retained_bytes(right, next_depth)?)
                .ok_or_else(|| {
                    anyhow::anyhow!("coherent snapshot metadata byte accounting overflow")
                })
        }
    }
}

/// Exact dynamic allocation capacity retained by one complete PDU82 response.
///
/// The producer ledger already bounds each field and the aggregate. Recounting
/// borrowed response values here is allocation-free and transfers that exact
/// ownership into the connection's dedicated complete-snapshot byte tranche.
fn coherent_snapshot_metadata_retained_bytes(
    response: &ListPanesCoherentResponse,
) -> anyhow::Result<usize> {
    let ListPanesCoherentOutcome::Snapshot(snapshot) = &response.outcome else {
        return Ok(0);
    };
    let panes = &snapshot.panes;
    let mut total = 0usize;
    let mut add = |bytes: usize| -> anyhow::Result<()> {
        total = total.checked_add(bytes).ok_or_else(|| {
            anyhow::anyhow!("coherent snapshot metadata byte accounting overflow")
        })?;
        Ok(())
    };
    for tab in &panes.tabs {
        add(pane_node_metadata_retained_bytes(tab, 1)?)?;
    }
    for title in &panes.tab_titles {
        add(title.capacity())?;
    }
    for title in panes.window_titles.values() {
        add(title.capacity())?;
    }
    for floating in &panes.floating_panes {
        add(pane_entry_metadata_retained_bytes(&floating.pane)?)?;
    }
    Ok(total)
}

#[derive(Debug)]
enum OrderedSnapshotFrameOwner {
    Reserved(EncodedOutboundFrame),
    Published,
}

#[derive(Debug)]
struct OrderedSnapshotQueueRejection {
    error: anyhow::Error,
    owner: OrderedSnapshotFrameOwner,
}

/// Reject a known-full control queue or a budget with no minimum snapshot
/// headroom before paying q-sized validation and serialization. This is an
/// advisory optimization only: exact reservation follows encoding outside the
/// coordinator lock, and terminal admission is checked again at publication.
fn preflight_ordered_snapshot_response(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &OutboundBudget,
) -> anyhow::Result<()> {
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    if item_tx.is_full() {
        admission.trip(RESPONSE_QUEUE_FAILURE);
        drop(admission);
        metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
        anyhow::bail!(
            "mux dispatch item queue is full (capacity \
             {DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY}); applying client backpressure"
        );
    }
    if let Err(limit) = budget.preflight(OutboundClass::Snapshot, 1) {
        admission.trip(OUTBOUND_BUDGET_OVERFLOW);
        drop(admission);
        return Err(outbound_budget_rejection(OutboundClass::Snapshot, limit));
    }
    Ok(())
}

/// Publish one already-validated and already-encoded PDU87 while preserving
/// control-slot priority and a dedicated transport batch boundary. The only
/// accepted owner was constructed from a typed PDU87 through the private
/// `OrderedSnapshotFence` encoder. Queue rejection returns the allocation to
/// the caller so it can be destroyed after releasing the coordinator lock.
fn queue_prepared_ordered_snapshot_response(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    frame: EncodedOutboundFrame,
) -> Result<(), OrderedSnapshotQueueRejection> {
    if terminal.is_tripped() {
        return Err(OrderedSnapshotQueueRejection {
            error: anyhow::anyhow!("mux dispatch connection is already terminal"),
            owner: OrderedSnapshotFrameOwner::Reserved(frame),
        });
    }
    let Some(admission) = terminal.admit() else {
        return Err(OrderedSnapshotQueueRejection {
            error: anyhow::anyhow!("mux dispatch connection is already terminal"),
            owner: OrderedSnapshotFrameOwner::Reserved(frame),
        });
    };
    let item = Item::WritePdu(WritePayload::Encoded(frame));
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(item)) => {
            admission.trip(RESPONSE_QUEUE_FAILURE);
            drop(admission);
            metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
            let Item::WritePdu(WritePayload::Encoded(frame)) = item else {
                unreachable!("ordered snapshot queue rejection changed item shape");
            };
            Err(OrderedSnapshotQueueRejection {
                error: anyhow::anyhow!(
                    "mux dispatch item queue is full (capacity \
                     {DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY}); applying client backpressure"
                ),
                owner: OrderedSnapshotFrameOwner::Reserved(frame),
            })
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(RESPONSE_QUEUE_FAILURE);
            drop(admission);
            metrics::counter!("mux.dispatch.response_enqueue_failure").increment(1);
            let Item::WritePdu(WritePayload::Encoded(frame)) = item else {
                unreachable!("ordered snapshot queue rejection changed item shape");
            };
            Err(OrderedSnapshotQueueRejection {
                error: anyhow::anyhow!("mux dispatch item queue is closed"),
                owner: OrderedSnapshotFrameOwner::Reserved(frame),
            })
        }
    }
}

fn queue_reserved_pdu(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
) -> anyhow::Result<()> {
    queue_reserved_pdu_with_emission_authority(
        item_tx,
        terminal,
        decoded,
        reservation,
        ServerEmissionAuthority::Ordinary,
    )
}

#[derive(Debug)]
struct ReservedTopologyItemQueueRejection {
    error: anyhow::Error,
    item: Box<Item>,
}

fn queue_reserved_pdu_with_emission_authority(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
    emission_authority: ServerEmissionAuthority,
) -> anyhow::Result<()> {
    match try_queue_reserved_pdu_with_emission_authority(
        item_tx,
        terminal,
        decoded,
        reservation,
        emission_authority,
    ) {
        Ok(()) => Ok(()),
        Err(rejected) => {
            let ReservedTopologyItemQueueRejection { error, item } = rejected;
            drop(item);
            Err(error)
        }
    }
}

/// Publish a topology PDU without consuming its owner on rejection. Callers
/// holding the coordinator state mutex must move the returned boxed owner to
/// an outside-the-lock retirement carrier before propagating the error.  The
/// indirection keeps this rejection-only `Result` small on the success path.
fn try_queue_reserved_pdu_with_emission_authority(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    decoded: Box<DecodedPdu>,
    reservation: OutboundReservation,
    emission_authority: ServerEmissionAuthority,
) -> Result<(), ReservedTopologyItemQueueRejection> {
    let item = Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu {
        decoded,
        reservation,
        emission_authority,
    }));
    let Item::WritePdu(WritePayload::Typed(typed)) = &item else {
        unreachable!("reserved topology PDU changed item shape");
    };
    if let Err(error) = validate_server_emission_authority(
        &typed.decoded.pdu,
        typed.decoded.serial,
        terminal,
        emission_authority,
    ) {
        return Err(ReservedTopologyItemQueueRejection {
            error,
            item: Box::new(item),
        });
    }
    let Some(admission) = terminal.admit() else {
        return Err(ReservedTopologyItemQueueRejection {
            error: anyhow::anyhow!("mux dispatch connection is already terminal"),
            item: Box::new(item),
        });
    };
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(item)) => {
            admission.trip(NOTIFICATION_QUEUE_OVERFLOW);
            drop(admission);
            metrics::counter!("mux.dispatch.notification_queue.full").increment(1);
            Err(ReservedTopologyItemQueueRejection {
                error: anyhow::anyhow!(
                    "mux dispatch topology queue is full (bulk capacity \
                     {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
                ),
                item: Box::new(item),
            })
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(NOTIFICATION_QUEUE_CLOSED);
            drop(admission);
            Err(ReservedTopologyItemQueueRejection {
                error: anyhow::anyhow!("mux dispatch topology queue is closed"),
                item: Box::new(item),
            })
        }
    }
}

fn queue_reserved_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    notification: MuxNotification,
    reservation: OutboundReservation,
) -> bool {
    match try_queue_reserved_notification(item_tx, terminal, notification, reservation) {
        Ok(()) => true,
        Err(rejected) => {
            drop(rejected.item);
            false
        }
    }
}

fn try_queue_reserved_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    notification: MuxNotification,
    reservation: OutboundReservation,
) -> Result<(), ReservedTopologyItemQueueRejection> {
    let item = Item::Notif(ReservedNotification {
        notification,
        reservation,
    });
    let Some(admission) = terminal.admit() else {
        return Err(ReservedTopologyItemQueueRejection {
            error: anyhow::anyhow!("mux dispatch connection is already terminal"),
            item: Box::new(item),
        });
    };
    match item_tx.try_send(item) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(item)) => {
            admission.trip(NOTIFICATION_QUEUE_OVERFLOW);
            drop(admission);
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
            Err(ReservedTopologyItemQueueRejection {
                error: anyhow::anyhow!(
                    "mux dispatch notification queue is full (bulk capacity \
                     {DISPATCH_ITEM_QUEUE_CAPACITY}); applying client backpressure"
                ),
                item: Box::new(item),
            })
        }
        Err(TrySendError::Closed(item)) => {
            admission.trip(NOTIFICATION_QUEUE_CLOSED);
            drop(admission);
            Err(ReservedTopologyItemQueueRejection {
                error: anyhow::anyhow!("mux dispatch notification queue is closed"),
                item: Box::new(item),
            })
        }
    }
}

fn queue_reserved_retained_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    notification: RetainedTopologyNotification,
    reservation: OutboundReservation,
) -> bool {
    match notification {
        RetainedTopologyNotification::Ordinary(notification) => {
            queue_reserved_notification(item_tx, terminal, notification, reservation)
        }
        RetainedTopologyNotification::WindowOrderChanged {
            legacy_resync_tab_id,
            ..
        }
        | RetainedTopologyNotification::WindowTopologyChanged {
            legacy_resync_tab_id,
            ..
        } => queue_reserved_pdu(
            item_tx,
            terminal,
            Box::new(DecodedPdu {
                pdu: Pdu::TabResized(codec::TabResized {
                    tab_id: legacy_resync_tab_id,
                }),
                serial: 0,
            }),
            reservation,
        )
        .is_ok(),
    }
}

fn queue_notification(
    item_tx: &Sender<Item>,
    terminal: &DispatchTerminal,
    budget: &Arc<OutboundBudget>,
    notification: MuxNotification,
) -> bool {
    let Some(admission) = terminal.admit() else {
        return false;
    };
    let reservation = match reserve_outbound_admitted(budget, OutboundClass::Bulk, 0) {
        Ok(reservation) => reservation,
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            let _ = outbound_budget_rejection(OutboundClass::Bulk, limit);
            return false;
        }
    };
    let item = Item::Notif(ReservedNotification {
        notification,
        reservation,
    });
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

#[derive(Debug)]
enum RetainedTopologyNotification {
    Ordinary(MuxNotification),
    /// Compact replacement for a frozen mux graph. Ordered-capable phases
    /// carry wire state converted and validated outside the coordinator
    /// mutex; legacy/coherent phases keep `None` and pay no q-sized work. The
    /// two local ids are the bounded legacy/coherent fallback authorities.
    WindowOrderChanged {
        window_id: usize,
        legacy_resync_tab_id: usize,
        ordered_window: Option<codec::OrderedWindowStateV1>,
    },
    WindowTopologyChanged {
        legacy_resync_tab_id: usize,
        ordered_windows: Option<Vec<codec::OrderedWindowStateV1>>,
    },
}

#[derive(Debug)]
struct PreparedTopologyNotification {
    notification: RetainedTopologyNotification,
    dynamic_bytes: usize,
}

#[derive(Debug)]
struct RetainedTopologyEvent {
    notification: RetainedTopologyNotification,
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

#[derive(Debug)]
struct TopologyEventInsertRejection {
    error: anyhow::Error,
    event: Box<RetainedTopologyEvent>,
}

impl TopologyEventBuffer {
    /// Validate duplicate/count admission while the coordinator state lock is
    /// still held and before the candidate acquires an outbound reservation.
    /// `insert` repeats this check so transfer paths cannot bypass it.
    fn preflight_insert(
        &self,
        revision: TopologyRevision,
        limits: TopologyRetentionLimits,
    ) -> anyhow::Result<usize> {
        if self.events.contains_key(&revision) {
            anyhow::bail!("duplicate mux topology revision {}", revision.get());
        }
        let next_len = self
            .events
            .len()
            .checked_add(1)
            .context("counting retained mux topology events")?;
        if next_len > limits.max_events {
            anyhow::bail!(
                "mux topology fence buffer would retain {next_len} events; maximum is {}",
                limits.max_events
            );
        }
        Ok(next_len)
    }

    fn insert(
        &mut self,
        event: RetainedTopologyEvent,
        limits: TopologyRetentionLimits,
    ) -> anyhow::Result<()> {
        match self.try_insert(event, limits) {
            Ok(()) => Ok(()),
            Err(rejected) => Err(rejected.error),
        }
    }

    fn try_insert(
        &mut self,
        event: RetainedTopologyEvent,
        limits: TopologyRetentionLimits,
    ) -> Result<(), TopologyEventInsertRejection> {
        let next_len = match self.preflight_insert(event.revision, limits) {
            Ok(next_len) => next_len,
            Err(error) => {
                return Err(TopologyEventInsertRejection {
                    error,
                    event: Box::new(event),
                });
            }
        };
        let Some(next_bytes) = self.retained_bytes.checked_add(event.retained_bytes) else {
            return Err(TopologyEventInsertRejection {
                error: anyhow::anyhow!("counting retained mux topology bytes"),
                event: Box::new(event),
            });
        };
        if next_bytes > limits.max_retained_bytes {
            return Err(TopologyEventInsertRejection {
                error: anyhow::anyhow!(
                    "mux topology fence buffer would retain {next_len} events and {next_bytes} bytes"
                ),
                event: Box::new(event),
            });
        }
        self.retained_bytes = next_bytes;
        self.events.insert(event.revision, event);
        metrics::histogram!("mux.dispatch.topology_fence.retained_events").record(next_len as f64);
        metrics::histogram!("mux.dispatch.topology_fence.retained_bytes").record(next_bytes as f64);
        Ok(())
    }

    fn remove(
        &mut self,
        revision: TopologyRevision,
    ) -> anyhow::Result<Option<RetainedTopologyEvent>> {
        let Some(event) = self.events.get(&revision) else {
            return Ok(None);
        };
        let next_retained_bytes = self
            .retained_bytes
            .checked_sub(event.retained_bytes)
            .context("decrementing retained mux topology bytes")?;
        let event = self
            .events
            .remove(&revision)
            .expect("borrowed topology revision must remain present");
        self.retained_bytes = next_retained_bytes;
        Ok(Some(event))
    }

    fn first_revision(&self) -> Option<TopologyRevision> {
        self.events.first_key_value().map(|(revision, _)| *revision)
    }

    fn pop_first(&mut self) -> anyhow::Result<Option<RetainedTopologyEvent>> {
        let Some(revision) = self.first_revision() else {
            return Ok(None);
        };
        self.remove(revision)
    }

    fn take_all(&mut self) -> impl Iterator<Item = RetainedTopologyEvent> {
        self.retained_bytes = 0;
        std::mem::take(&mut self.events).into_values()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TopologySubscriptionAuthority {
    session_incarnation: MuxSessionIncarnation,
    baseline_revision: TopologyRevision,
}

const fn ordered_snapshot_foundation() -> TopologyCapabilities {
    TopologyCapabilities::from_bits(
        TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
            | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
    )
}

/// Immutable proof that one exact PDU86/PDU87 cut established ordered-window
/// authority on this dispatch connection.
///
/// The handler receives only a copy of this proof for a PDU88 that dispatch
/// admitted. It cannot mint, replace, or retain the connection state itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EstablishedOrderedWindowAuthority {
    stream_id: TopologyStreamId,
    session_incarnation: MuxSessionIncarnation,
    domain_binding_id: codec::DomainBindingId,
    negotiated: TopologyCapabilities,
}

impl EstablishedOrderedWindowAuthority {
    fn try_new(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        domain_binding_id: codec::DomainBindingId,
        negotiated: TopologyCapabilities,
    ) -> anyhow::Result<Self> {
        if stream_id.as_bytes().iter().all(|byte| *byte == 0)
            || session_incarnation.as_bytes().iter().all(|byte| *byte == 0)
            || domain_binding_id.as_bytes().iter().all(|byte| *byte == 0)
        {
            anyhow::bail!(
                "cannot establish ordered-window authority with a zero stream, session, or binding identity"
            );
        }
        negotiated
            .validate()
            .context("validating established ordered-window capabilities")?;
        if !negotiated.contains(ordered_snapshot_foundation()) {
            anyhow::bail!(
                "cannot establish ordered-window authority without fenced and ordered-stream capabilities"
            );
        }
        Ok(Self {
            stream_id,
            session_incarnation,
            domain_binding_id,
            negotiated,
        })
    }

    pub(crate) const fn stream_id(self) -> TopologyStreamId {
        self.stream_id
    }

    pub(crate) const fn session_incarnation(self) -> MuxSessionIncarnation {
        self.session_incarnation
    }

    pub(crate) const fn domain_binding_id(self) -> codec::DomainBindingId {
        self.domain_binding_id
    }

    pub(crate) const fn negotiated(self) -> TopologyCapabilities {
        self.negotiated
    }
}

#[cfg(test)]
pub(crate) fn established_ordered_window_authority_for_test(
    stream_id: TopologyStreamId,
    session_incarnation: MuxSessionIncarnation,
    domain_binding_id: codec::DomainBindingId,
    negotiated: TopologyCapabilities,
) -> EstablishedOrderedWindowAuthority {
    EstablishedOrderedWindowAuthority::try_new(
        stream_id,
        session_incarnation,
        domain_binding_id,
        negotiated,
    )
    .expect("test ordered-window authority must satisfy the dispatch foundation")
}

#[derive(Debug)]
struct EstablishedOrderedWindowStream {
    authority: EstablishedOrderedWindowAuthority,
    /// The exact successfully fenced PDU86. Refresh is idempotent only when
    /// this complete bounded request repeats; comparing just the negotiated
    /// intersection would silently accept changed offered/required bits.
    request: Arc<codec::ListPanesOrderedV1>,
}

#[derive(Debug)]
enum TopologyFenceKind {
    Coherent {
        negotiated: TopologyCapabilities,
    },
    Ordered {
        /// Allocation identity is the exact fence generation.  A PDU87
        /// validator may clone this `Arc`, release the coordinator lock for
        /// q-sized snapshot validation, then use `Arc::ptr_eq` after
        /// reacquiring the lock to reject a stale response even when a client
        /// reused the same serial and byte-identical PDU86.
        request: Arc<codec::ListPanesOrderedV1>,
        negotiated: TopologyCapabilities,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrderedFenceOutcomeAuthority {
    Snapshot {
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: TopologyRevision,
    },
    Contended,
    RevisionExhausted,
    Unsupported,
}

#[derive(Debug)]
enum DeferredTopologyOwner {
    Prepared(PreparedTopologyNotification),
    Event(RetainedTopologyEvent),
    OrderedWindow(codec::OrderedWindowStateV1),
    OrderedWindows(Vec<codec::OrderedWindowStateV1>),
    Item(Item),
}

fn defer_topology_owner(
    retired_owners: &mut Vec<DeferredTopologyOwner>,
    owner: DeferredTopologyOwner,
) {
    assert!(
        retired_owners.len() < retired_owners.capacity(),
        "bounded topology retirement carrier exhausted"
    );
    retired_owners.push(owner);
}

#[derive(Debug)]
struct PreparedOrderedFenceResponse {
    stream_id: TopologyStreamId,
    negotiated: TopologyCapabilities,
    outcome: OrderedFenceOutcomeAuthority,
    /// Exact PDU87 wire frame produced after request correlation and the private
    /// ordered-snapshot permit were validated, but before reacquiring the
    /// coordinator lock for FIFO admission.
    frame: OrderedSnapshotFrameOwner,
    /// The complete prior/next phase and every q-bearing owner retired while
    /// `TopologyStreamState` is locked. This carrier itself is created outside
    /// the lock and is destroyed only by `release_after_unlock`.
    retired_phase: Option<TopologyStreamPhase>,
    retired_owners: Vec<DeferredTopologyOwner>,
}

impl PreparedOrderedFenceResponse {
    fn publish(
        &mut self,
        item_tx: &Sender<Item>,
        terminal: &DispatchTerminal,
    ) -> anyhow::Result<()> {
        let owner = std::mem::replace(&mut self.frame, OrderedSnapshotFrameOwner::Published);
        let OrderedSnapshotFrameOwner::Reserved(frame) = owner else {
            self.frame = owner;
            anyhow::bail!("ordered snapshot frame did not retain unpublished authority");
        };
        match queue_prepared_ordered_snapshot_response(item_tx, terminal, frame) {
            Ok(()) => Ok(()),
            Err(rejected) => {
                self.frame = rejected.owner;
                Err(rejected.error)
            }
        }
    }

    fn release_after_unlock(self) {
        let Self {
            frame,
            retired_phase,
            retired_owners,
            ..
        } = self;
        match frame {
            OrderedSnapshotFrameOwner::Reserved(frame) => drop(frame),
            OrderedSnapshotFrameOwner::Published => {}
        }
        drop(retired_phase);
        for owner in retired_owners {
            match owner {
                DeferredTopologyOwner::Prepared(notification) => drop(notification),
                DeferredTopologyOwner::Event(event) => drop(event),
                DeferredTopologyOwner::OrderedWindow(window) => drop(window),
                DeferredTopologyOwner::OrderedWindows(windows) => drop(windows),
                DeferredTopologyOwner::Item(item) => drop(item),
            }
        }
    }
}

#[derive(Debug)]
enum TopologyFencePrior {
    Legacy,
    Established {
        snapshot_revision: TopologyRevision,
        next_revision: Option<TopologyRevision>,
        ordered: Option<EstablishedOrderedWindowStream>,
    },
}

impl TopologyFencePrior {
    fn published_revision(&self) -> Option<TopologyRevision> {
        match self {
            Self::Legacy => None,
            Self::Established {
                snapshot_revision,
                next_revision,
                ..
            } => Some(
                match next_revision {
                    Some(next_revision) => TopologyRevision::new(
                        next_revision
                            .get()
                            .checked_sub(1)
                            .unwrap_or_else(|| snapshot_revision.get()),
                    ),
                    None => TopologyRevision::new(u64::MAX),
                }
                .max(*snapshot_revision),
            ),
        }
    }

    fn ordered_authority(&self) -> Option<EstablishedOrderedWindowAuthority> {
        match self {
            Self::Legacy => None,
            Self::Established { ordered, .. } => ordered.as_ref().map(|ordered| ordered.authority),
        }
    }
}

#[derive(Debug)]
struct TopologyFenceInFlight {
    serial: u64,
    kind: TopologyFenceKind,
    prior: TopologyFencePrior,
    buffer: TopologyEventBuffer,
}

#[derive(Debug)]
struct EstablishedTopologyStream {
    snapshot_revision: TopologyRevision,
    next_revision: Option<TopologyRevision>,
    ordered: Option<EstablishedOrderedWindowStream>,
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
enum OrderedDeliveryGeneration {
    Fencing(Arc<codec::ListPanesOrderedV1>),
    Established(EstablishedOrderedWindowAuthority),
}

impl OrderedDeliveryGeneration {
    fn capture(phase: &TopologyStreamPhase) -> Option<Self> {
        match phase {
            TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                kind:
                    TopologyFenceKind::Ordered {
                        request,
                        negotiated,
                    },
                ..
            }) if negotiated.contains(ordered_snapshot_foundation())
                && negotiated.contains(request.required) =>
            {
                Some(Self::Fencing(Arc::clone(request)))
            }
            TopologyStreamPhase::Established(EstablishedTopologyStream {
                ordered: Some(ordered),
                ..
            }) => Some(Self::Established(ordered.authority)),
            TopologyStreamPhase::Legacy
            | TopologyStreamPhase::Fencing(_)
            | TopologyStreamPhase::Established(_)
            | TopologyStreamPhase::Exhausted => None,
        }
    }

    fn is_current(&self, phase: &TopologyStreamPhase) -> bool {
        match (self, phase) {
            (
                Self::Fencing(expected),
                TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                    kind: TopologyFenceKind::Ordered { request, .. },
                    ..
                }),
            ) => Arc::ptr_eq(expected, request),
            (
                Self::Established(expected),
                TopologyStreamPhase::Established(EstablishedTopologyStream {
                    ordered: Some(ordered),
                    ..
                }),
            ) => expected == &ordered.authority,
            _ => false,
        }
    }
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
    retention_limits: TopologyRetentionLimits,
    state: ParkingMutex<TopologyStreamState>,
    #[cfg(test)]
    before_ordered_snapshot_publish: ParkingMutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_ordered_snapshot_validation: ParkingMutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_ordered_snapshot_publish: ParkingMutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_unilateral_render_publish: ParkingMutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl TopologyStreamCoordinator {
    fn new(item_tx: Sender<Item>, terminal: DispatchTerminal, stream_id: TopologyStreamId) -> Self {
        Self::new_with_retention_limits(
            item_tx,
            terminal,
            stream_id,
            TopologyRetentionLimits::default(),
        )
    }

    fn new_with_retention_limits(
        item_tx: Sender<Item>,
        terminal: DispatchTerminal,
        stream_id: TopologyStreamId,
        retention_limits: TopologyRetentionLimits,
    ) -> Self {
        Self {
            item_tx,
            terminal,
            outbound_budget: Arc::new(OutboundBudget::default()),
            stream_id,
            retention_limits,
            state: ParkingMutex::new(TopologyStreamState::default()),
            #[cfg(test)]
            before_ordered_snapshot_publish: ParkingMutex::new(None),
            #[cfg(test)]
            after_ordered_snapshot_validation: ParkingMutex::new(None),
            #[cfg(test)]
            after_ordered_snapshot_publish: ParkingMutex::new(None),
            #[cfg(test)]
            after_unilateral_render_publish: ParkingMutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_before_ordered_snapshot_publish_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_ordered_snapshot_publish.lock() = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_after_ordered_snapshot_publish_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.after_ordered_snapshot_publish.lock() = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_after_ordered_snapshot_validation_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.after_ordered_snapshot_validation.lock() = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn set_after_unilateral_render_publish_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.after_unilateral_render_publish.lock() = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn run_before_ordered_snapshot_publish_hook(&self) {
        let hook = self.before_ordered_snapshot_publish.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn run_after_ordered_snapshot_validation_hook(&self) {
        let hook = self.after_ordered_snapshot_validation.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn run_after_ordered_snapshot_publish_hook(&self) {
        let hook = self.after_ordered_snapshot_publish.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    fn run_after_unilateral_render_publish_hook(&self) {
        let hook = self.after_unilateral_render_publish.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn discard_retained_state(&self) {
        let retired = {
            let mut state = self.state.lock();
            (
                std::mem::take(&mut state.prebind),
                std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted),
            )
        };
        // A retained phase can own thousands of dynamic payloads and outbound
        // reservations. Release all of them after the coordinator lock so
        // destructors never nest the budget mutex under `state`.
        drop(retired);
    }

    /// Convert every request-boundary rejection into one sticky connection
    /// failure and revoke all retained topology authority. Callers invoke this
    /// only after the rejecting operation has returned, so q-sized snapshots,
    /// buffered events, and their budget reservations are destroyed outside
    /// both the coordinator and terminal-admission locks.
    fn reject_client_request(&self) {
        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
        self.discard_retained_state();
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
            if !queue_reserved_retained_notification(
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

    fn begin_fence_admitted(&self, serial: u64, request: &ListPanesCoherent) -> anyhow::Result<()> {
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
        if matches!(
            &state.phase,
            TopologyStreamPhase::Established(EstablishedTopologyStream {
                ordered: Some(_),
                ..
            })
        ) {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!(
                "a coherent-only snapshot cannot replace established ordered-window capabilities"
            );
        }
        let phase = std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
        state.phase = match phase {
            TopologyStreamPhase::Legacy => TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                serial,
                kind: TopologyFenceKind::Coherent { negotiated },
                prior: TopologyFencePrior::Legacy,
                buffer: TopologyEventBuffer::default(),
            }),
            TopologyStreamPhase::Established(established) => {
                debug_assert!(established.ordered.is_none());
                TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                    serial,
                    kind: TopologyFenceKind::Coherent { negotiated },
                    prior: TopologyFencePrior::Established {
                        snapshot_revision: established.snapshot_revision,
                        next_revision: established.next_revision,
                        ordered: established.ordered,
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

    fn begin_ordered_fence(
        &self,
        serial: u64,
        request: &codec::ListPanesOrderedV1,
    ) -> anyhow::Result<()> {
        self.with_live_result(|| {
            self.begin_ordered_fence_admitted(
                serial,
                request,
                TopologyCapabilities::SERVER_SUPPORTED,
            )
        })
    }

    #[cfg(test)]
    fn begin_ordered_fence_with_server_capabilities(
        &self,
        serial: u64,
        request: &codec::ListPanesOrderedV1,
        server_supported: TopologyCapabilities,
    ) -> anyhow::Result<()> {
        self.with_live_result(|| {
            self.begin_ordered_fence_admitted(serial, request, server_supported)
        })
    }

    fn begin_ordered_fence_admitted(
        &self,
        serial: u64,
        request: &codec::ListPanesOrderedV1,
        server_supported: TopologyCapabilities,
    ) -> anyhow::Result<()> {
        request
            .validate()
            .context("validating PDU86 before ordered topology fencing")?;
        server_supported
            .validate()
            .context("validating ordered topology server capability mask")?;
        let negotiated = request.supported.intersection(server_supported);
        let request = Arc::new(request.clone());
        let mut state = self.state.lock();
        if state.subscription.is_none() {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("ordered mux topology fence began before its subscription was bound");
        }
        if let TopologyStreamPhase::Established(EstablishedTopologyStream {
            ordered: Some(ordered),
            ..
        }) = &state.phase
        {
            if ordered.request.as_ref() != request.as_ref()
                || ordered.authority.negotiated != negotiated
            {
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!(
                    "ordered-window request or negotiated capability change revoked the connection authority"
                );
            }
        }

        let phase = std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
        state.phase = match phase {
            TopologyStreamPhase::Legacy => TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                serial,
                kind: TopologyFenceKind::Ordered {
                    request: Arc::clone(&request),
                    negotiated,
                },
                prior: TopologyFencePrior::Legacy,
                buffer: TopologyEventBuffer::default(),
            }),
            TopologyStreamPhase::Established(established) => {
                TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                    serial,
                    kind: TopologyFenceKind::Ordered {
                        request: Arc::clone(&request),
                        negotiated,
                    },
                    prior: TopologyFencePrior::Established {
                        snapshot_revision: established.snapshot_revision,
                        next_revision: established.next_revision,
                        ordered: established.ordered,
                    },
                    buffer: established.buffer,
                })
            }
            TopologyStreamPhase::Fencing(in_flight) => {
                state.phase = TopologyStreamPhase::Fencing(in_flight);
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!("overlapping mux topology snapshot requests")
            }
            TopologyStreamPhase::Exhausted => {
                state.phase = TopologyStreamPhase::Exhausted;
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
                anyhow::bail!("mux topology stream is exhausted")
            }
        };
        metrics::counter!(
            "mux.dispatch.ordered_fence.negotiation.total",
            "outcome" => if negotiated.contains(ordered_snapshot_foundation())
                && negotiated.contains(request.required)
            {
                "admitted"
            } else {
                "unsupported"
            }
        )
        .increment(1);
        Ok(())
    }

    fn admit_ordered_reorder(
        &self,
        request: &codec::ReorderWindowTabsV1,
    ) -> anyhow::Result<EstablishedOrderedWindowAuthority> {
        self.with_live_result(|| {
            // Returning this immutable token is the PDU88 admission
            // linearization point. A refresh or revocation that acquires the
            // coordinator mutex later is ordered after this already-admitted
            // request; the handler must still enforce session, binding,
            // digest, and CAS authority before mutation.
            let state = self.state.lock();
            let TopologyStreamPhase::Established(established) = &state.phase else {
                anyhow::bail!(
                    "ordered-window stream has not been established by a successful PDU87"
                );
            };
            let authority = established
                .ordered
                .as_ref()
                .map(|ordered| ordered.authority)
                .context("ordered-window stream has not been established by a successful PDU87")?;
            if !authority
                .negotiated
                .contains(TopologyCapabilities::WINDOW_REORDER_CAS_V1)
            {
                anyhow::bail!(
                    "ordered-window reorder capability is not established on this stream"
                );
            }
            if request.stream_id != authority.stream_id {
                anyhow::bail!("ordered-window reorder targets a stale or foreign topology stream");
            }
            Ok(authority)
        })
    }

    fn queue_response(
        &self,
        decoded: DecodedPdu,
        delivery_class: PduDeliveryClass,
    ) -> anyhow::Result<()> {
        // FIFO admission is the linearization point for every PduSender. Once
        // publication succeeds, a later terminal transition must not rewrite
        // that fact into `Err`; callers use an error as proof that ownership
        // remained local. Operations that lose the terminal race before
        // publication still fail inside `queue_response_admitted`.
        if self.terminal.is_tripped() {
            self.discard_retained_state();
            anyhow::bail!("mux dispatch connection is already terminal");
        }
        let result = self.queue_response_admitted(decoded, delivery_class);
        if self.terminal.is_tripped() {
            self.discard_retained_state();
        }
        result
    }

    /// Queue one unilateral legacy render effect with an exact admission
    /// result. Unlike the general topology response wrapper, this path must
    /// never turn a successful FIFO publication into `Err` merely because the
    /// connection becomes terminal immediately afterwards: legacy render
    /// rollback interprets `Err` as proof that publication did not occur.
    fn queue_unilateral_render_response(
        &self,
        decoded: DecodedPdu,
        delivery_class: PduDeliveryClass,
    ) -> anyhow::Result<()> {
        let DecodedPdu { pdu, serial } = decoded;
        let valid_class = matches!(
            (&pdu, delivery_class),
            (
                Pdu::GetPaneRenderChangesResponse(_),
                PduDeliveryClass::Control | PduDeliveryClass::Bulk,
            ) | (
                Pdu::SetPalette(_) | Pdu::NotifyAlert(_),
                PduDeliveryClass::Bulk,
            )
        );
        if serial != 0 || !valid_class {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            self.discard_retained_state();
            anyhow::bail!("invalid PDU or delivery class reached unilateral render admission");
        }
        let result = queue_response_pdu(
            &self.item_tx,
            &self.terminal,
            &self.outbound_budget,
            pdu,
            serial,
            delivery_class,
        );
        if result.is_err() && self.terminal.is_tripped() {
            self.discard_retained_state();
        }
        result?;
        #[cfg(test)]
        self.run_after_unilateral_render_publish_hook();
        if self.terminal.is_tripped() {
            self.discard_retained_state();
        }
        Ok(())
    }

    fn queue_response_admitted(
        &self,
        decoded: DecodedPdu,
        delivery_class: PduDeliveryClass,
    ) -> anyhow::Result<()> {
        if delivery_class == PduDeliveryClass::Bulk {
            let DecodedPdu { pdu, serial } = decoded;
            if serial != 0
                || matches!(
                    &pdu,
                    Pdu::ListPanesCoherentResponse(_) | Pdu::ListPanesOrderedV1Response(_)
                )
            {
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!("non-unilateral mux response was classified as bulk");
            }
            return queue_response_pdu(
                &self.item_tx,
                &self.terminal,
                &self.outbound_budget,
                pdu,
                serial,
                delivery_class,
            );
        }

        let DecodedPdu { pdu, serial } = decoded;
        match pdu {
            Pdu::ListPanesCoherentResponse(response) => {
                let mut state = self.state.lock();
                self.complete_fence_response(&mut state, serial, response)
                    .inspect_err(|_| self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE))
            }
            Pdu::ListPanesOrderedV1Response(response) => self
                .validate_and_complete_ordered_fence_response(serial, response)
                .inspect_err(|_| self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE)),
            other => {
                let mut state = self.state.lock();
                if matches!(
                    &state.phase,
                    TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                        serial: fence_serial,
                        kind: TopologyFenceKind::Ordered { .. },
                        ..
                    }) if *fence_serial == serial
                ) {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!("ordered mux snapshot fence produced a non-PDU87 response");
                }
                let phase = std::mem::replace(&mut state.phase, TopologyStreamPhase::Exhausted);
                let mut response_was_published = false;
                let result = (|| {
                    match phase {
                        TopologyStreamPhase::Fencing(in_flight) if in_flight.serial == serial => {
                            debug_assert!(matches!(
                                &in_flight.kind,
                                TopologyFenceKind::Coherent { .. }
                            ));
                            queue_response_pdu(
                                &self.item_tx,
                                &self.terminal,
                                &self.outbound_budget,
                                other,
                                serial,
                                delivery_class,
                            )?;
                            response_was_published = true;
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
                                delivery_class,
                            )?;
                            response_was_published = true;
                        }
                    }
                    Ok(())
                })();
                if response_was_published {
                    if result.is_err() {
                        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    }
                    Ok(())
                } else {
                    result.inspect_err(|_| self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE))
                }
            }
        }
    }

    /// Validate the potentially q-sized PDU87 snapshot without holding the
    /// coordinator mutex.  The request `Arc` is a fence-generation token: the
    /// completion path must observe the identical allocation after reacquiring
    /// the mutex, not merely an equal serial or request value.
    fn validate_and_complete_ordered_fence_response(
        &self,
        serial: u64,
        response: codec::ListPanesOrderedV1Response,
    ) -> anyhow::Result<()> {
        let (request_generation, subscription_authority) = {
            let state = self.state.lock();
            let subscription = state
                .subscription
                .context("ordered mux snapshot completed before subscription binding")?;
            let TopologyStreamPhase::Fencing(in_flight) = &state.phase else {
                anyhow::bail!("ordered mux snapshot response arrived without an active fence");
            };
            let TopologyFenceKind::Ordered {
                request,
                negotiated,
            } = &in_flight.kind
            else {
                anyhow::bail!("ordered mux snapshot response crossed a coherent request fence");
            };
            if in_flight.serial != serial
                || response.stream_id != self.stream_id
                || response.negotiated != *negotiated
            {
                anyhow::bail!("ordered mux snapshot response did not match its request fence");
            }
            (Arc::clone(request), subscription)
        };

        preflight_ordered_snapshot_response(&self.item_tx, &self.terminal, &self.outbound_budget)?;
        let response = response
            .validate_for_request_owned(&request_generation)
            .context("validating request-correlated PDU87 at the dispatch fence")?;
        if let codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) =
            &response.as_response().outcome
        {
            validate_ordered_snapshot_projection(snapshot)
                .context("validating PDU87 pane/order projection at the dispatch fence")?;
        }

        let outcome = match &response.as_response().outcome {
            codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) => {
                OrderedFenceOutcomeAuthority::Snapshot {
                    session_incarnation: snapshot.session_incarnation,
                    snapshot_revision: snapshot.topology_revision,
                }
            }
            codec::ListPanesOrderedV1Outcome::Contended { .. } => {
                OrderedFenceOutcomeAuthority::Contended
            }
            codec::ListPanesOrderedV1Outcome::RevisionExhausted => {
                OrderedFenceOutcomeAuthority::RevisionExhausted
            }
            codec::ListPanesOrderedV1Outcome::Unsupported { .. } => {
                OrderedFenceOutcomeAuthority::Unsupported
            }
        };
        let response_stream_id = response.as_response().stream_id;
        let response_negotiated = response.as_response().negotiated;
        let frame = AuthorizedOrderedSnapshotFrame::encode(response, serial, &self.terminal)?
            .reserve(&self.terminal, &self.outbound_budget)
            .context("reserving exact PDU87 frame outside the coordinator lock")?;
        let mut retired_owners = Vec::new();
        retired_owners
            .try_reserve_exact(self.retention_limits.max_events.saturating_add(1))
            .map_err(|error| {
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                anyhow::anyhow!("allocating ordered-fence deferred-release carrier failed: {error}")
            })?;
        // The encoded frame is the sole q-sized outbound owner from this point;
        // the private encoder released the typed pane/order graph before FIFO
        // admission begins.
        let mut prepared = PreparedOrderedFenceResponse {
            stream_id: response_stream_id,
            negotiated: response_negotiated,
            outcome,
            frame: OrderedSnapshotFrameOwner::Reserved(frame),
            retired_phase: None,
            retired_owners,
        };

        #[cfg(test)]
        self.run_before_ordered_snapshot_publish_hook();

        let result = {
            let mut state = self.state.lock();
            self.complete_ordered_fence_response(
                &mut state,
                serial,
                request_generation,
                subscription_authority,
                &mut prepared,
            )
        };
        let response_was_published = matches!(prepared.frame, OrderedSnapshotFrameOwner::Published);
        // Revalidation and queue failures deliberately return ownership here;
        // a maximum-size frame must never be deallocated under `state`.
        prepared.release_after_unlock();
        if response_was_published {
            if let Err(err) = result {
                // PDU87 publication is the response linearization point. A
                // later buffered-event failure terminates this connection,
                // but returning Err would falsely tell PduSender callers that
                // the response remained local and could be retried.
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                log::error!(
                    "ordered mux snapshot response was published before terminal topology settlement failed: {err:#}"
                );
            }
            Ok(())
        } else {
            result
        }
    }

    fn complete_fence_response(
        &self,
        state: &mut TopologyStreamState,
        serial: u64,
        response: ListPanesCoherentResponse,
    ) -> anyhow::Result<()> {
        let mut response_was_published = false;
        let result = self.complete_fence_response_with_admission_witness(
            state,
            serial,
            response,
            &mut response_was_published,
        );
        if response_was_published {
            if result.is_err() {
                // As for PDU87, the coherent response has already crossed the
                // FIFO boundary. Retire the stream, retain Ok as the exact
                // admission result, and never invite a duplicate response.
                // This function runs under the coordinator mutex; terminal
                // reason telemetry is deliberately used instead of logging
                // while holding that hot lock.
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            }
            Ok(())
        } else {
            result
        }
    }

    fn complete_fence_response_with_admission_witness(
        &self,
        state: &mut TopologyStreamState,
        serial: u64,
        response: ListPanesCoherentResponse,
        response_was_published: &mut bool,
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
                    PduDeliveryClass::Control,
                );
            }
            state.phase = TopologyStreamPhase::Exhausted;
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("coherent mux snapshot response arrived without an active fence");
        };
        let TopologyFenceKind::Coherent { negotiated } = &in_flight.kind else {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("coherent mux snapshot response crossed an ordered request fence");
        };
        if in_flight.serial != serial || *negotiated != response.negotiated {
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
            ListPanesCoherentOutcome::Snapshot(snapshot) => FenceOutcomeAuthority::Snapshot {
                session_incarnation: snapshot.session_incarnation,
                snapshot_revision: snapshot.snapshot_revision,
            },
            ListPanesCoherentOutcome::Contended { .. } => FenceOutcomeAuthority::Contended,
            ListPanesCoherentOutcome::RevisionExhausted => FenceOutcomeAuthority::RevisionExhausted,
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
                let snapshot_regresses_prior = in_flight
                    .prior
                    .published_revision()
                    .is_some_and(|prior| snapshot_revision < prior);
                let revision_namespace_exhausted = snapshot_revision.get() == u64::MAX;
                if wrong_session_incarnation
                    || snapshot_predates_subscription
                    || snapshot_regresses_prior
                    || revision_namespace_exhausted
                {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!(
                        "coherent mux snapshot authority did not match the connection subscription"
                    );
                }
                let retained_metadata = coherent_snapshot_metadata_retained_bytes(&response)?;
                queue_response_pdu_with_accounting(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                    OutboundClass::Snapshot,
                    retained_metadata,
                    ServerEmissionAuthority::Ordinary,
                )?;
                *response_was_published = true;

                let mut established = EstablishedTopologyStream {
                    snapshot_revision,
                    next_revision: snapshot_revision
                        .get()
                        .checked_add(1)
                        .map(TopologyRevision::new),
                    ordered: None,
                    buffer: TopologyEventBuffer::default(),
                };
                for event in in_flight.buffer.take_all() {
                    if event.revision > snapshot_revision {
                        established
                            .buffer
                            .insert(event, self.retention_limits)
                            .inspect_err(|_| {
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
                    PduDeliveryClass::Control,
                )?;
                *response_was_published = true;
                state.phase = self.restore_prior(in_flight)?;
            }
            FenceOutcomeAuthority::RevisionExhausted => {
                queue_response_pdu(
                    &self.item_tx,
                    &self.terminal,
                    &self.outbound_budget,
                    Pdu::ListPanesCoherentResponse(response),
                    serial,
                    PduDeliveryClass::Control,
                )?;
                *response_was_published = true;
                state.phase = TopologyStreamPhase::Exhausted;
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
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

    fn complete_ordered_fence_response(
        &self,
        state: &mut TopologyStreamState,
        serial: u64,
        request_generation: Arc<codec::ListPanesOrderedV1>,
        subscription_authority: TopologySubscriptionAuthority,
        prepared: &mut PreparedOrderedFenceResponse,
    ) -> anyhow::Result<()> {
        debug_assert!(prepared.retired_phase.is_none());
        prepared.retired_phase = Some(std::mem::replace(
            &mut state.phase,
            TopologyStreamPhase::Exhausted,
        ));
        if state.subscription != Some(subscription_authority) {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!(
                "ordered mux snapshot response crossed a subscription-authority generation"
            );
        }
        let Some(TopologyStreamPhase::Fencing(in_flight)) = prepared.retired_phase.as_ref() else {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("ordered mux snapshot response arrived without an active fence");
        };
        let negotiated = match &in_flight.kind {
            TopologyFenceKind::Ordered {
                request,
                negotiated,
            } if Arc::ptr_eq(request, &request_generation) => *negotiated,
            TopologyFenceKind::Ordered { .. } => {
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!(
                    "ordered mux snapshot response targeted a superseded fence generation"
                );
            }
            TopologyFenceKind::Coherent { .. } => {
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                anyhow::bail!("ordered mux snapshot response crossed a coherent request fence");
            }
        };
        if in_flight.serial != serial
            || prepared.stream_id != self.stream_id
            || prepared.negotiated != negotiated
        {
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            anyhow::bail!("ordered mux snapshot response did not match its request fence");
        }

        let outcome = prepared.outcome;

        match outcome {
            OrderedFenceOutcomeAuthority::Snapshot {
                session_incarnation,
                snapshot_revision,
            } => {
                let subscription = subscription_authority;
                let wrong_session_incarnation =
                    session_incarnation != subscription.session_incarnation;
                let snapshot_predates_subscription =
                    snapshot_revision < subscription.baseline_revision;
                let snapshot_regresses_prior = in_flight
                    .prior
                    .published_revision()
                    .is_some_and(|prior| snapshot_revision < prior);
                let revision_namespace_exhausted = snapshot_revision.get() == u64::MAX;
                if wrong_session_incarnation
                    || snapshot_predates_subscription
                    || snapshot_regresses_prior
                    || revision_namespace_exhausted
                {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!(
                        "ordered mux snapshot authority did not match the connection subscription"
                    );
                }

                let authority = EstablishedOrderedWindowAuthority::try_new(
                    self.stream_id,
                    session_incarnation,
                    request_generation.domain_binding_id,
                    negotiated,
                )?;
                if in_flight
                    .prior
                    .ordered_authority()
                    .is_some_and(|prior| prior != authority)
                {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!(
                        "ordered mux snapshot attempted to replace immutable connection authority"
                    );
                }

                #[cfg(test)]
                self.run_after_ordered_snapshot_validation_hook();
                prepared.publish(&self.item_tx, &self.terminal)?;
                #[cfg(test)]
                self.run_after_ordered_snapshot_publish_hook();

                loop {
                    let first_revision = match prepared.retired_phase.as_ref() {
                        Some(TopologyStreamPhase::Fencing(in_flight)) => {
                            in_flight.buffer.first_revision()
                        }
                        _ => unreachable!("validated ordered fence changed phase"),
                    };
                    if first_revision.is_none_or(|revision| revision > snapshot_revision) {
                        break;
                    }
                    let event = match prepared.retired_phase.as_mut() {
                        Some(TopologyStreamPhase::Fencing(in_flight)) => in_flight
                            .buffer
                            .pop_first()?
                            .expect("observed first ordered-fence revision must remain present"),
                        _ => unreachable!("validated ordered fence changed phase"),
                    };
                    defer_topology_owner(
                        &mut prepared.retired_owners,
                        DeferredTopologyOwner::Event(event),
                    );
                    metrics::counter!(
                        "mux.dispatch.ordered_fence.events.total",
                        "outcome" => "snapshot_subsumed"
                    )
                    .increment(1);
                }

                let phase = prepared
                    .retired_phase
                    .take()
                    .expect("ordered fence retirement carrier must own its phase");
                let TopologyStreamPhase::Fencing(in_flight) = phase else {
                    unreachable!("validated ordered fence changed phase");
                };
                prepared.retired_phase = Some(TopologyStreamPhase::Established(
                    EstablishedTopologyStream {
                        snapshot_revision,
                        next_revision: snapshot_revision
                            .get()
                            .checked_add(1)
                            .map(TopologyRevision::new),
                        ordered: Some(EstablishedOrderedWindowStream {
                            authority,
                            request: request_generation,
                        }),
                        buffer: in_flight.buffer,
                    },
                ));
                self.drain_retired_ordered_established(prepared)?;
                state.phase = prepared
                    .retired_phase
                    .take()
                    .expect("successful ordered fence must retain its established phase");
            }
            OrderedFenceOutcomeAuthority::Contended => {
                prepared.publish(&self.item_tx, &self.terminal)?;
                self.restore_retired_ordered_prior(prepared)?;
                state.phase = prepared
                    .retired_phase
                    .take()
                    .expect("contended ordered fence must restore its prior phase");
            }
            OrderedFenceOutcomeAuthority::RevisionExhausted => {
                prepared.publish(&self.item_tx, &self.terminal)?;
                self.terminal.trip(TOPOLOGY_REVISION_EXHAUSTED);
            }
            OrderedFenceOutcomeAuthority::Unsupported => {
                if negotiated.contains(ordered_snapshot_foundation())
                    && negotiated.contains(request_generation.required)
                {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    anyhow::bail!(
                        "ordered mux snapshot became unsupported after its fence was admitted"
                    );
                }
                prepared.publish(&self.item_tx, &self.terminal)?;
                self.restore_retired_ordered_prior(prepared)?;
                state.phase = prepared
                    .retired_phase
                    .take()
                    .expect("unsupported ordered fence must restore its prior phase");
            }
        }
        Ok(())
    }

    fn restore_retired_ordered_prior(
        &self,
        prepared: &mut PreparedOrderedFenceResponse,
    ) -> anyhow::Result<()> {
        let prior_is_legacy = matches!(
            prepared.retired_phase.as_ref(),
            Some(TopologyStreamPhase::Fencing(TopologyFenceInFlight {
                prior: TopologyFencePrior::Legacy,
                ..
            }))
        );
        if prior_is_legacy {
            loop {
                let event = match prepared.retired_phase.as_mut() {
                    Some(TopologyStreamPhase::Fencing(in_flight)) => {
                        in_flight.buffer.pop_first()?
                    }
                    _ => unreachable!("validated ordered fence changed phase"),
                };
                let Some(event) = event else {
                    break;
                };
                self.queue_retired_ordered_event_as_legacy(event, &mut prepared.retired_owners)?;
            }
            let phase = prepared
                .retired_phase
                .take()
                .expect("ordered fence retirement carrier must own its phase");
            let TopologyStreamPhase::Fencing(in_flight) = phase else {
                unreachable!("validated ordered fence changed phase");
            };
            debug_assert!(in_flight.buffer.events.is_empty());
            prepared.retired_phase = Some(TopologyStreamPhase::Legacy);
            return Ok(());
        }

        let phase = prepared
            .retired_phase
            .take()
            .expect("ordered fence retirement carrier must own its phase");
        let TopologyStreamPhase::Fencing(in_flight) = phase else {
            prepared.retired_phase = Some(phase);
            anyhow::bail!("ordered fence prior restore lost its in-flight phase");
        };
        let TopologyFencePrior::Established {
            snapshot_revision,
            next_revision,
            ordered,
        } = in_flight.prior
        else {
            unreachable!("ordered fence prior restore changed authority class");
        };
        prepared.retired_phase = Some(TopologyStreamPhase::Established(
            EstablishedTopologyStream {
                snapshot_revision,
                next_revision,
                ordered,
                buffer: in_flight.buffer,
            },
        ));
        self.drain_retired_ordered_established(prepared)
    }

    fn drain_retired_ordered_established(
        &self,
        prepared: &mut PreparedOrderedFenceResponse,
    ) -> anyhow::Result<()> {
        loop {
            let (next_revision, ordered_authority) = match prepared.retired_phase.as_ref() {
                Some(TopologyStreamPhase::Established(established)) => (
                    established.next_revision,
                    established
                        .ordered
                        .as_ref()
                        .map(|ordered| ordered.authority),
                ),
                _ => {
                    anyhow::bail!("ordered fence drain lost its retirement-owned established phase")
                }
            };
            let Some(next_revision) = next_revision else {
                return Ok(());
            };
            let event = match prepared.retired_phase.as_mut() {
                Some(TopologyStreamPhase::Established(established)) => {
                    established.buffer.remove(next_revision)?
                }
                _ => unreachable!("ordered established phase changed during drain"),
            };
            let Some(event) = event else {
                return Ok(());
            };
            self.queue_stamped_event_deferred(
                ordered_authority,
                event,
                &mut prepared.retired_owners,
            )?;
            metrics::counter!(
                "mux.dispatch.topology_fence.events.total",
                "outcome" => "replayed"
            )
            .increment(1);
            let next = next_revision
                .get()
                .checked_add(1)
                .map(TopologyRevision::new);
            let Some(TopologyStreamPhase::Established(established)) =
                prepared.retired_phase.as_mut()
            else {
                unreachable!("ordered established phase changed during drain");
            };
            established.next_revision = next;
        }
    }

    fn queue_retired_ordered_event_as_legacy(
        &self,
        event: RetainedTopologyEvent,
        retired_owners: &mut Vec<DeferredTopologyOwner>,
    ) -> anyhow::Result<()> {
        let RetainedTopologyEvent {
            notification,
            reservation,
            ..
        } = event;
        let result = match notification {
            RetainedTopologyNotification::Ordinary(notification) => {
                try_queue_reserved_notification(
                    &self.item_tx,
                    &self.terminal,
                    notification,
                    reservation,
                )
            }
            RetainedTopologyNotification::WindowOrderChanged {
                legacy_resync_tab_id,
                ordered_window,
                ..
            } => {
                if let Some(ordered_window) = ordered_window {
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::OrderedWindow(ordered_window),
                    );
                }
                try_queue_reserved_pdu_with_emission_authority(
                    &self.item_tx,
                    &self.terminal,
                    Box::new(DecodedPdu {
                        pdu: Pdu::TabResized(codec::TabResized {
                            tab_id: legacy_resync_tab_id,
                        }),
                        serial: 0,
                    }),
                    reservation,
                    ServerEmissionAuthority::Ordinary,
                )
            }
            RetainedTopologyNotification::WindowTopologyChanged {
                legacy_resync_tab_id,
                ordered_windows,
            } => {
                if let Some(ordered_windows) = ordered_windows {
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::OrderedWindows(ordered_windows),
                    );
                }
                try_queue_reserved_pdu_with_emission_authority(
                    &self.item_tx,
                    &self.terminal,
                    Box::new(DecodedPdu {
                        pdu: Pdu::TabResized(codec::TabResized {
                            tab_id: legacy_resync_tab_id,
                        }),
                        serial: 0,
                    }),
                    reservation,
                    ServerEmissionAuthority::Ordinary,
                )
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(rejected) => {
                defer_topology_owner(retired_owners, DeferredTopologyOwner::Item(*rejected.item));
                Err(rejected.error)
            }
        }
    }

    fn restore_prior(
        &self,
        mut in_flight: TopologyFenceInFlight,
    ) -> anyhow::Result<TopologyStreamPhase> {
        match in_flight.prior {
            TopologyFencePrior::Legacy => {
                for event in in_flight.buffer.take_all() {
                    if !queue_reserved_retained_notification(
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
                ordered,
            } => {
                let mut established = EstablishedTopologyStream {
                    snapshot_revision,
                    next_revision,
                    ordered,
                    buffer: in_flight.buffer,
                };
                self.drain_established(&mut established)?;
                Ok(TopologyStreamPhase::Established(established))
            }
        }
    }

    fn drain_established(&self, established: &mut EstablishedTopologyStream) -> anyhow::Result<()> {
        while let Some(next_revision) = established.next_revision {
            let Some(event) = established.buffer.remove(next_revision)? else {
                break;
            };
            self.queue_stamped_event(
                established
                    .ordered
                    .as_ref()
                    .map(|ordered| ordered.authority),
                event,
            )?;
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

    fn drain_established_deferred(
        &self,
        established: &mut EstablishedTopologyStream,
        retired_owners: &mut Vec<DeferredTopologyOwner>,
    ) -> anyhow::Result<()> {
        while let Some(next_revision) = established.next_revision {
            let Some(event) = established.buffer.remove(next_revision)? else {
                break;
            };
            self.queue_stamped_event_deferred(
                established
                    .ordered
                    .as_ref()
                    .map(|ordered| ordered.authority),
                event,
                retired_owners,
            )?;
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

    fn queue_stamped_event(
        &self,
        ordered_authority: Option<EstablishedOrderedWindowAuthority>,
        event: RetainedTopologyEvent,
    ) -> anyhow::Result<()> {
        let RetainedTopologyEvent {
            notification,
            revision,
            reservation,
            ..
        } = event;
        let (pdu, emission_authority) = match (ordered_authority, notification) {
            (
                Some(authority),
                RetainedTopologyNotification::WindowOrderChanged {
                    ordered_window: Some(ordered_window),
                    ..
                },
            ) if authority
                .negotiated
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1) =>
            {
                let event = codec::WindowOrderEventV1 {
                    protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                    stream_id: authority.stream_id,
                    session_incarnation: authority.session_incarnation,
                    topology_revision: revision,
                    windows: vec![ordered_window],
                };
                (
                    Pdu::WindowOrderEventV1(event),
                    ServerEmissionAuthority::OrderedStreamEvent,
                )
            }
            (
                Some(authority),
                RetainedTopologyNotification::WindowTopologyChanged {
                    ordered_windows: Some(windows),
                    ..
                },
            ) if authority
                .negotiated
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1) =>
            {
                (
                    Pdu::WindowOrderEventV1(codec::WindowOrderEventV1 {
                        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                        stream_id: authority.stream_id,
                        session_incarnation: authority.session_incarnation,
                        topology_revision: revision,
                        windows,
                    }),
                    ServerEmissionAuthority::OrderedStreamEvent,
                )
            }
            (
                Some(_),
                RetainedTopologyNotification::WindowOrderChanged {
                    ordered_window: None,
                    ..
                },
            ) => {
                anyhow::bail!(
                    "ordered-window authority reached an event without preconverted PDU90 state"
                );
            }
            (
                Some(_),
                RetainedTopologyNotification::WindowTopologyChanged {
                    ordered_windows: None,
                    ..
                },
            ) => {
                anyhow::bail!(
                    "ordered-window authority reached a transaction without preconverted PDU90 state"
                );
            }
            (_, notification) => (
                Pdu::TopologyEvent(TopologyEvent {
                    stream_id: self.stream_id,
                    revision,
                    event: into_topology_event_kind(notification)?,
                }),
                ServerEmissionAuthority::Ordinary,
            ),
        };
        queue_reserved_pdu_with_emission_authority(
            &self.item_tx,
            &self.terminal,
            Box::new(DecodedPdu { pdu, serial: 0 }),
            reservation,
            emission_authority,
        )
    }

    fn queue_stamped_event_deferred(
        &self,
        ordered_authority: Option<EstablishedOrderedWindowAuthority>,
        event: RetainedTopologyEvent,
        retired_owners: &mut Vec<DeferredTopologyOwner>,
    ) -> anyhow::Result<()> {
        let notification_is_topology = match &event.notification {
            RetainedTopologyNotification::Ordinary(notification) => notification.is_topology(),
            RetainedTopologyNotification::WindowOrderChanged { .. }
            | RetainedTopologyNotification::WindowTopologyChanged { .. } => true,
        };
        if !notification_is_topology {
            defer_topology_owner(retired_owners, DeferredTopologyOwner::Event(event));
            anyhow::bail!("non-topology mux notification carried a topology revision");
        }
        let ordered_event_has_no_wire_authority = matches!(
            (&ordered_authority, &event.notification),
            (
                Some(authority),
                RetainedTopologyNotification::WindowOrderChanged { .. }
                    | RetainedTopologyNotification::WindowTopologyChanged { .. }
            ) if !authority
                .negotiated
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1)
        );
        let ordered_event_lost_frozen_state = matches!(
            (&ordered_authority, &event.notification),
            (
                Some(_),
                RetainedTopologyNotification::WindowOrderChanged {
                    ordered_window: None,
                    ..
                } | RetainedTopologyNotification::WindowTopologyChanged {
                    ordered_windows: None,
                    ..
                }
            )
        );
        if ordered_event_has_no_wire_authority || ordered_event_lost_frozen_state {
            defer_topology_owner(retired_owners, DeferredTopologyOwner::Event(event));
            anyhow::bail!(
                "ordered-window authority reached an event without permitted frozen PDU90 state"
            );
        }

        let RetainedTopologyEvent {
            notification,
            revision,
            reservation,
            ..
        } = event;
        let (pdu, emission_authority) = match (ordered_authority, notification) {
            (
                Some(authority),
                RetainedTopologyNotification::WindowOrderChanged {
                    ordered_window: Some(ordered_window),
                    ..
                },
            ) if authority
                .negotiated
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1) =>
            {
                (
                    Pdu::WindowOrderEventV1(codec::WindowOrderEventV1 {
                        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                        stream_id: authority.stream_id,
                        session_incarnation: authority.session_incarnation,
                        topology_revision: revision,
                        windows: vec![ordered_window],
                    }),
                    ServerEmissionAuthority::OrderedStreamEvent,
                )
            }
            (
                Some(authority),
                RetainedTopologyNotification::WindowTopologyChanged {
                    ordered_windows: Some(windows),
                    ..
                },
            ) if authority
                .negotiated
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1) =>
            {
                (
                    Pdu::WindowOrderEventV1(codec::WindowOrderEventV1 {
                        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                        stream_id: authority.stream_id,
                        session_incarnation: authority.session_incarnation,
                        topology_revision: revision,
                        windows,
                    }),
                    ServerEmissionAuthority::OrderedStreamEvent,
                )
            }
            (
                None,
                RetainedTopologyNotification::WindowOrderChanged {
                    window_id,
                    ordered_window,
                    ..
                },
            ) => {
                if let Some(ordered_window) = ordered_window {
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::OrderedWindow(ordered_window),
                    );
                }
                (
                    Pdu::TopologyEvent(TopologyEvent {
                        stream_id: self.stream_id,
                        revision,
                        event: TopologyEventKind::WindowInvalidated { window_id },
                    }),
                    ServerEmissionAuthority::Ordinary,
                )
            }
            (
                None,
                RetainedTopologyNotification::WindowTopologyChanged {
                    legacy_resync_tab_id,
                    ordered_windows,
                },
            ) => {
                if let Some(ordered_windows) = ordered_windows {
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::OrderedWindows(ordered_windows),
                    );
                }
                (
                    Pdu::TopologyEvent(TopologyEvent {
                        stream_id: self.stream_id,
                        revision,
                        event: TopologyEventKind::TabResized {
                            tab_id: legacy_resync_tab_id,
                        },
                    }),
                    ServerEmissionAuthority::Ordinary,
                )
            }
            (_, notification) => (
                Pdu::TopologyEvent(TopologyEvent {
                    stream_id: self.stream_id,
                    revision,
                    event: into_topology_event_kind(notification)?,
                }),
                ServerEmissionAuthority::Ordinary,
            ),
        };
        match try_queue_reserved_pdu_with_emission_authority(
            &self.item_tx,
            &self.terminal,
            Box::new(DecodedPdu { pdu, serial: 0 }),
            reservation,
            emission_authority,
        ) {
            Ok(()) => Ok(()),
            Err(rejected) => {
                defer_topology_owner(retired_owners, DeferredTopologyOwner::Item(*rejected.item));
                Err(rejected.error)
            }
        }
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
                let accepted = queue_notification(
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
        let notification = match notification {
            MuxNotification::WindowOrderChanged { window, .. } => {
                return self.on_window_order_topology_notification(window, revision);
            }
            MuxNotification::WindowTopologyChanged(change) => {
                return self.on_window_topology_notification(change, revision);
            }
            notification => notification,
        };
        let notification = match prepare_retained_topology_notification(notification) {
            Ok(notification) => notification,
            Err(err) => {
                log::error!("failed to prepare mux topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                return false;
            }
        };
        let mut retired_owners = Vec::with_capacity(2);
        let accepted = {
            let mut state = self.state.lock();
            self.admit_prepared_topology_notification(
                &mut state,
                notification,
                revision,
                &mut retired_owners,
            )
        };
        drop(retired_owners);
        accepted
    }

    fn on_window_order_topology_notification(
        &self,
        window: mux::window::FrozenWindowOrder,
        revision: TopologyRevision,
    ) -> bool {
        let window_id = window.window_id();
        let legacy_resync_tab_id = window.active_tab_id().unwrap_or(0);
        let mut retired_owners = Vec::with_capacity(2);
        let state = self.state.lock();
        let ordered_generation = OrderedDeliveryGeneration::capture(&state.phase);

        let Some(ordered_generation) = ordered_generation else {
            let notification = PreparedTopologyNotification {
                notification: RetainedTopologyNotification::WindowOrderChanged {
                    window_id,
                    legacy_resync_tab_id,
                    ordered_window: None,
                },
                dynamic_bytes: 0,
            };
            let mut state = state;
            let accepted = self.admit_prepared_topology_notification(
                &mut state,
                notification,
                revision,
                &mut retired_owners,
            );
            drop(state);
            // `FrozenWindowOrder` can own q tab Arcs. Drop it only after the
            // coordinator guard so the last-reference destructor is never
            // hidden inside the state critical section.
            drop(window);
            drop(retired_owners);
            return accepted;
        };

        drop(state);
        // This is the only q-sized conversion in dispatch. It is paid only by
        // a future-enabled ordered fence/stream, never by legacy, coherent, or
        // currently dormant PDU86 clients.
        let ordered_window = match frozen_window_order_to_codec(&window) {
            Ok(ordered_window) => ordered_window,
            Err(err) => {
                log::error!("failed to prepare ordered-window topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                return false;
            }
        };
        drop(window);
        let notification = match prepared_window_order_notification(
            window_id,
            legacy_resync_tab_id,
            Some(ordered_window),
        ) {
            Ok(notification) => notification,
            Err(err) => {
                log::error!("failed to account ordered-window topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                return false;
            }
        };

        let mut state = self.state.lock();
        if !ordered_generation.is_current(&state.phase) {
            // A fence may complete while conversion runs. The compact window
            // state is authority-neutral, so classify it against the current
            // phase/revision below; never publish using the stale generation.
            metrics::counter!(
                "mux.dispatch.ordered_event.preparation.total",
                "outcome" => "generation_changed"
            )
            .increment(1);
        }
        if OrderedDeliveryGeneration::capture(&state.phase).is_none() {
            let fallback = PreparedTopologyNotification {
                notification: RetainedTopologyNotification::WindowOrderChanged {
                    window_id,
                    legacy_resync_tab_id,
                    ordered_window: None,
                },
                dynamic_bytes: 0,
            };
            let accepted = self.admit_prepared_topology_notification(
                &mut state,
                fallback,
                revision,
                &mut retired_owners,
            );
            drop(state);
            // The optimistic q-sized representation is not useful to the
            // phase that won the race. Release its Vec only after leaving the
            // coordinator critical section; no legacy/coherent buffer pays
            // ordered-state accounting or allocator work.
            drop(notification);
            drop(retired_owners);
            return accepted;
        }
        let accepted = self.admit_prepared_topology_notification(
            &mut state,
            notification,
            revision,
            &mut retired_owners,
        );
        drop(state);
        drop(retired_owners);
        accepted
    }

    fn on_window_topology_notification(
        &self,
        change: mux::FrozenWindowTopologyChange,
        revision: TopologyRevision,
    ) -> bool {
        let legacy_resync_tab_id = change.legacy_resync_tab_id();
        let mut retired_owners = Vec::with_capacity(2);
        let state = self.state.lock();
        let ordered_generation = OrderedDeliveryGeneration::capture(&state.phase);

        let Some(ordered_generation) = ordered_generation else {
            let notification = PreparedTopologyNotification {
                notification: RetainedTopologyNotification::WindowTopologyChanged {
                    legacy_resync_tab_id,
                    ordered_windows: None,
                },
                dynamic_bytes: 0,
            };
            let mut state = state;
            let accepted = self.admit_prepared_topology_notification(
                &mut state,
                notification,
                revision,
                &mut retired_owners,
            );
            drop(state);
            drop(change);
            drop(retired_owners);
            return accepted;
        };

        if !change.removed_windows().is_empty() {
            drop(state);
            log::error!(
                "ordered-window stream cannot encode atomic window retirement; failing closed"
            );
            self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
            return false;
        }

        drop(state);
        let mut ordered_windows = Vec::new();
        if let Err(error) = ordered_windows.try_reserve_exact(change.windows().len()) {
            log::error!("failed to reserve frozen window transaction conversion: {error}");
            self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
            return false;
        }
        for window in change.windows() {
            match frozen_window_order_to_codec(window) {
                Ok(window) => ordered_windows.push(window),
                Err(err) => {
                    log::error!("failed to prepare frozen window transaction: {err:#}");
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    return false;
                }
            }
        }
        drop(change);
        let notification = match prepared_window_topology_notification(
            legacy_resync_tab_id,
            Some(ordered_windows),
        ) {
            Ok(notification) => notification,
            Err(err) => {
                log::error!("failed to account frozen window transaction: {err:#}");
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                return false;
            }
        };

        let mut state = self.state.lock();
        if !ordered_generation.is_current(&state.phase) {
            metrics::counter!(
                "mux.dispatch.ordered_event.preparation.total",
                "outcome" => "generation_changed"
            )
            .increment(1);
        }
        if OrderedDeliveryGeneration::capture(&state.phase).is_none() {
            let fallback = PreparedTopologyNotification {
                notification: RetainedTopologyNotification::WindowTopologyChanged {
                    legacy_resync_tab_id,
                    ordered_windows: None,
                },
                dynamic_bytes: 0,
            };
            let accepted = self.admit_prepared_topology_notification(
                &mut state,
                fallback,
                revision,
                &mut retired_owners,
            );
            drop(state);
            drop(notification);
            drop(retired_owners);
            return accepted;
        }
        let accepted = self.admit_prepared_topology_notification(
            &mut state,
            notification,
            revision,
            &mut retired_owners,
        );
        drop(state);
        drop(retired_owners);
        accepted
    }

    fn admit_prepared_topology_notification(
        &self,
        state: &mut TopologyStreamState,
        notification: PreparedTopologyNotification,
        revision: TopologyRevision,
        retired_owners: &mut Vec<DeferredTopologyOwner>,
    ) -> bool {
        if let Some(subscription) = state.subscription {
            // The subscription baseline is the causal cut at which this
            // connection became visible. A notification whose revision was
            // reserved before or at that cut may arrive after binding, but it
            // is already represented by the baseline and must never leak as a
            // legacy predecessor or enter a later fence buffer.
            if revision <= subscription.baseline_revision {
                defer_topology_owner(
                    retired_owners,
                    DeferredTopologyOwner::Prepared(notification),
                );
                return true;
            }
        } else {
            if let Err(err) = state
                .prebind
                .preflight_insert(revision, self.retention_limits)
            {
                log::error!("failed to preflight pre-bind mux topology event: {err:#}");
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                defer_topology_owner(
                    retired_owners,
                    DeferredTopologyOwner::Prepared(notification),
                );
                return false;
            }
            let event = match try_retained_topology_event(
                notification,
                revision,
                &self.terminal,
                &self.outbound_budget,
            ) {
                Ok(event) => event,
                Err(rejected) => {
                    log::error!(
                        "failed to retain pre-bind mux topology event: {:#}",
                        rejected.error
                    );
                    self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Prepared(rejected.notification),
                    );
                    return false;
                }
            };
            if let Err(rejected) = state.prebind.try_insert(event, self.retention_limits) {
                log::error!(
                    "failed to retain pre-bind mux topology event: {:#}",
                    rejected.error
                );
                self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                defer_topology_owner(
                    retired_owners,
                    DeferredTopologyOwner::Event(*rejected.event),
                );
                return false;
            }
            return true;
        }

        // Keep the coordinator lock through phase classification and any
        // enqueue. Otherwise this callback can observe Legacy, lose the lock
        // to begin_fence, and append an unstamped notification after the fence
        // has begun. Whichever side acquires this lock first defines whether
        // the event is a predecessor legacy frame or a retained successor.
        let phase = &mut state.phase;
        match phase {
            TopologyStreamPhase::Legacy => {
                let event = match try_retained_topology_event(
                    notification,
                    revision,
                    &self.terminal,
                    &self.outbound_budget,
                ) {
                    Ok(event) => event,
                    Err(rejected) => {
                        log::error!(
                            "failed to retain legacy mux topology event: {:#}",
                            rejected.error
                        );
                        self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        defer_topology_owner(
                            retired_owners,
                            DeferredTopologyOwner::Prepared(rejected.notification),
                        );
                        return false;
                    }
                };
                if let Err(err) = self.queue_retired_ordered_event_as_legacy(event, retired_owners)
                {
                    log::error!("failed to queue legacy mux topology event: {err:#}");
                    false
                } else {
                    true
                }
            }
            TopologyStreamPhase::Fencing(in_flight) => {
                if let Err(err) = in_flight
                    .buffer
                    .preflight_insert(revision, self.retention_limits)
                {
                    log::error!("failed to preflight in-flight mux topology event: {err:#}");
                    self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Prepared(notification),
                    );
                    return false;
                }
                let event = match try_retained_topology_event(
                    notification,
                    revision,
                    &self.terminal,
                    &self.outbound_budget,
                ) {
                    Ok(event) => event,
                    Err(rejected) => {
                        log::error!(
                            "failed to retain in-flight mux topology event: {:#}",
                            rejected.error
                        );
                        self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        defer_topology_owner(
                            retired_owners,
                            DeferredTopologyOwner::Prepared(rejected.notification),
                        );
                        return false;
                    }
                };
                if let Err(rejected) = in_flight.buffer.try_insert(event, self.retention_limits) {
                    log::error!(
                        "failed to retain in-flight mux topology event: {:#}",
                        rejected.error
                    );
                    self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Event(*rejected.event),
                    );
                    false
                } else {
                    true
                }
            }
            TopologyStreamPhase::Established(established) => {
                if revision <= established.snapshot_revision {
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Prepared(notification),
                    );
                    return true;
                }
                let Some(next_revision) = established.next_revision else {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Prepared(notification),
                    );
                    return false;
                };
                if revision < next_revision {
                    self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                    defer_topology_owner(
                        retired_owners,
                        DeferredTopologyOwner::Prepared(notification),
                    );
                    return false;
                }
                if revision == next_revision {
                    let event = match try_retained_topology_event(
                        notification,
                        revision,
                        &self.terminal,
                        &self.outbound_budget,
                    ) {
                        Ok(event) => event,
                        Err(rejected) => {
                            log::error!(
                                "failed to retain contiguous mux topology event: {:#}",
                                rejected.error
                            );
                            self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                            defer_topology_owner(
                                retired_owners,
                                DeferredTopologyOwner::Prepared(rejected.notification),
                            );
                            return false;
                        }
                    };
                    if let Err(err) = self.queue_stamped_event_deferred(
                        established
                            .ordered
                            .as_ref()
                            .map(|ordered| ordered.authority),
                        event,
                        retired_owners,
                    ) {
                        log::error!("failed to enqueue contiguous mux topology event: {err:#}");
                        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                        return false;
                    }
                    established.next_revision = next_revision
                        .get()
                        .checked_add(1)
                        .map(TopologyRevision::new);
                    if let Err(err) = self.drain_established_deferred(established, retired_owners) {
                        log::error!("failed to drain reordered mux topology events: {err:#}");
                        self.terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
                        return false;
                    }
                } else {
                    if let Err(err) = established
                        .buffer
                        .preflight_insert(revision, self.retention_limits)
                    {
                        log::error!("failed to preflight gapped mux topology event: {err:#}");
                        self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        defer_topology_owner(
                            retired_owners,
                            DeferredTopologyOwner::Prepared(notification),
                        );
                        return false;
                    }
                    let event = match try_retained_topology_event(
                        notification,
                        revision,
                        &self.terminal,
                        &self.outbound_budget,
                    ) {
                        Ok(event) => event,
                        Err(rejected) => {
                            log::error!(
                                "failed to retain gapped mux topology event: {:#}",
                                rejected.error
                            );
                            self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                            defer_topology_owner(
                                retired_owners,
                                DeferredTopologyOwner::Prepared(rejected.notification),
                            );
                            return false;
                        }
                    };
                    if let Err(rejected) =
                        established.buffer.try_insert(event, self.retention_limits)
                    {
                        log::error!(
                            "failed to retain gapped mux topology event: {:#}",
                            rejected.error
                        );
                        self.terminal.trip(TOPOLOGY_BUFFER_OVERFLOW);
                        defer_topology_owner(
                            retired_owners,
                            DeferredTopologyOwner::Event(*rejected.event),
                        );
                        return false;
                    }
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
                defer_topology_owner(
                    retired_owners,
                    DeferredTopologyOwner::Prepared(notification),
                );
                false
            }
        }
    }
}

#[derive(Debug)]
struct RetainedTopologyEventRejection {
    error: anyhow::Error,
    notification: PreparedTopologyNotification,
}

fn try_retained_topology_event(
    notification: PreparedTopologyNotification,
    revision: TopologyRevision,
    terminal: &DispatchTerminal,
    outbound_budget: &Arc<OutboundBudget>,
) -> Result<RetainedTopologyEvent, RetainedTopologyEventRejection> {
    if matches!(
        &notification.notification,
        RetainedTopologyNotification::Ordinary(
            MuxNotification::WindowOrderChanged { .. } | MuxNotification::WindowTopologyChanged(_)
        )
    ) {
        return Err(RetainedTopologyEventRejection {
            error: anyhow::anyhow!(
                "unprepared frozen window-order graph reached retained topology admission"
            ),
            notification,
        });
    }
    let Some(retained_bytes) =
        RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES.checked_add(notification.dynamic_bytes)
    else {
        return Err(RetainedTopologyEventRejection {
            error: anyhow::anyhow!("counting retained mux topology event bytes"),
            notification,
        });
    };
    let reservation = match reserve_outbound(
        terminal,
        outbound_budget,
        OutboundClass::Topology,
        retained_bytes,
    ) {
        Ok(reservation) => reservation,
        Err(error) => {
            return Err(RetainedTopologyEventRejection {
                error,
                notification,
            });
        }
    };
    Ok(RetainedTopologyEvent {
        notification: notification.notification,
        revision,
        retained_bytes,
        reservation,
    })
}

fn prepare_retained_topology_notification(
    notification: MuxNotification,
) -> anyhow::Result<PreparedTopologyNotification> {
    let (notification, dynamic_bytes) = match notification {
        MuxNotification::WindowOrderChanged { window, .. } => {
            let window_id = window.window_id();
            let legacy_resync_tab_id = window.active_tab_id().unwrap_or(0);
            return prepared_window_order_notification(window_id, legacy_resync_tab_id, None);
        }
        MuxNotification::WindowTopologyChanged(change) => {
            return prepared_window_topology_notification(change.legacy_resync_tab_id(), None);
        }
        notification => {
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
                | MuxNotification::FloatingPaneSpawnCommitted(_)
                | MuxNotification::PaneRemoved(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_)
                | MuxNotification::WindowInvalidated(_)
                | MuxNotification::Empty
                | MuxNotification::TabAddedToWindow { .. }
                | MuxNotification::PaneFocused(_)
                | MuxNotification::TabResized(_) => 0,
                MuxNotification::WindowOrderChanged { .. } => {
                    unreachable!("window-order notifications are converted by the outer match")
                }
                MuxNotification::WindowTopologyChanged(_) => {
                    unreachable!("window topology transactions are converted by the outer match")
                }
                MuxNotification::PaneOutput(_)
                | MuxNotification::SynchronizedOutput { .. }
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::Alert { .. }
                | MuxNotification::AssignClipboard { .. }
                | MuxNotification::SaveToDownloads { .. } => {
                    anyhow::bail!("non-topology mux notification carried a topology revision")
                }
            };
            (
                RetainedTopologyNotification::Ordinary(notification),
                dynamic_bytes,
            )
        }
    };
    Ok(PreparedTopologyNotification {
        notification,
        dynamic_bytes,
    })
}

fn prepared_window_order_notification(
    window_id: usize,
    legacy_resync_tab_id: usize,
    ordered_window: Option<codec::OrderedWindowStateV1>,
) -> anyhow::Result<PreparedTopologyNotification> {
    let dynamic_bytes = match ordered_window.as_ref() {
        Some(window) => window
            .ordered_tab_ids
            .capacity()
            .checked_mul(std::mem::size_of::<codec::RemoteTabId>())
            .context("counting retained ordered-window wire bytes")?,
        None => 0,
    };
    Ok(PreparedTopologyNotification {
        notification: RetainedTopologyNotification::WindowOrderChanged {
            window_id,
            legacy_resync_tab_id,
            ordered_window,
        },
        dynamic_bytes,
    })
}

fn prepared_window_topology_notification(
    legacy_resync_tab_id: usize,
    ordered_windows: Option<Vec<codec::OrderedWindowStateV1>>,
) -> anyhow::Result<PreparedTopologyNotification> {
    let dynamic_bytes = ordered_windows.as_ref().map_or(Ok(0), |windows| {
        windows.iter().try_fold(
            windows
                .capacity()
                .checked_mul(std::mem::size_of::<codec::OrderedWindowStateV1>())
                .context("counting retained ordered-window transaction vector")?,
            |bytes, window| {
                bytes
                    .checked_add(
                        window
                            .ordered_tab_ids
                            .capacity()
                            .checked_mul(std::mem::size_of::<codec::RemoteTabId>())
                            .context("counting retained ordered-window transaction tabs")?,
                    )
                    .context("counting retained ordered-window transaction bytes")
            },
        )
    })?;
    Ok(PreparedTopologyNotification {
        notification: RetainedTopologyNotification::WindowTopologyChanged {
            legacy_resync_tab_id,
            ordered_windows,
        },
        dynamic_bytes,
    })
}

fn into_topology_event_kind(
    notification: RetainedTopologyNotification,
) -> anyhow::Result<TopologyEventKind> {
    let notification = match notification {
        RetainedTopologyNotification::WindowOrderChanged { window_id, .. } => {
            return Ok(TopologyEventKind::WindowInvalidated { window_id });
        }
        RetainedTopologyNotification::WindowTopologyChanged {
            legacy_resync_tab_id,
            ..
        } => {
            return Ok(TopologyEventKind::TabResized {
                tab_id: legacy_resync_tab_id,
            });
        }
        RetainedTopologyNotification::Ordinary(notification) => notification,
    };
    let event = match notification {
        MuxNotification::PaneAdded(pane_id) => TopologyEventKind::PaneAdded { pane_id },
        MuxNotification::FloatingPaneSpawnCommitted(spawn) => {
            TopologyEventKind::FloatingPaneSpawned {
                pane_id: spawn.pane_id(),
                tab_id: spawn.tab_id(),
                window_id: spawn.window_id(),
            }
        }
        MuxNotification::PaneRemoved(pane_id) => TopologyEventKind::PaneRemoved { pane_id },
        MuxNotification::WindowCreated(window_id) => TopologyEventKind::WindowCreated { window_id },
        MuxNotification::WindowRemoved(window_id) => TopologyEventKind::WindowRemoved { window_id },
        MuxNotification::WindowInvalidated(window_id) => {
            TopologyEventKind::WindowInvalidated { window_id }
        }
        MuxNotification::WindowTopologyChanged(change) => TopologyEventKind::TabResized {
            tab_id: change.legacy_resync_tab_id(),
        },
        // Production preparation converts this variant before the
        // coordinator lock. Preserve a fail-safe fallback for any internal
        // caller that deliberately constructs an ordinary retained value.
        MuxNotification::WindowOrderChanged { window, .. } => {
            TopologyEventKind::WindowInvalidated {
                window_id: window.window_id(),
            }
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
    validate_server_emission_authority(&pdu, 0, terminal, ServerEmissionAuthority::Ordinary)?;
    prepare_pending_outbound_batch(
        WritePayload::Typed(ReservedDecodedPdu {
            decoded: Box::new(DecodedPdu { pdu, serial: 0 }),
            reservation,
            emission_authority: ServerEmissionAuthority::Ordinary,
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
    _reservations: Vec<OutboundReservation>,
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

fn reweight_accounted_batch(
    terminal: &DispatchTerminal,
    reservations: &mut [OutboundReservation],
    retained_bytes: usize,
) -> anyhow::Result<()> {
    let Some(retained_class) = reservations
        .iter()
        .find(|reservation| reservation.accounts_retained_bytes())
        .map(|reservation| reservation.retained_class)
    else {
        return Ok(());
    };
    let Some(admission) = terminal.admit() else {
        anyhow::bail!("mux dispatch connection is already terminal");
    };
    match OutboundBudget::reweight_accounted_batch(reservations, retained_bytes) {
        Ok(()) => {
            drop(admission);
            Ok(())
        }
        Err(limit) => {
            admission.trip(OUTBOUND_BUDGET_OVERFLOW);
            drop(admission);
            metrics::counter!(
                "mux.dispatch.outbound_budget.rejected",
                "class" => retained_class.label(),
                "limit" => limit.label(),
            )
            .increment(1);
            anyhow::bail!(
                "encoded mux {} delivery exceeded the {} bound",
                retained_class.label(),
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
        WritePayload::Encoded(frame) => {
            frame.authority.validate(terminal)?;
            Ok(frame)
        }
        WritePayload::Typed(mut typed) => {
            let is_coherent_snapshot = matches!(
                &typed.decoded.pdu,
                Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                    outcome: ListPanesCoherentOutcome::Snapshot(_),
                    ..
                })
            );
            let authority = EncodedPduAuthority::capture(
                &typed.decoded.pdu,
                typed.decoded.serial,
                typed.emission_authority,
            );
            authority.validate(terminal)?;
            let encoded = typed
                .decoded
                .pdu
                .encode_frame_with_mode(typed.decoded.serial, compression_mode);
            if is_coherent_snapshot {
                if let Err(error) = &encoded {
                    record_snapshot_body_limit_mismatch("pdu82", error);
                }
            }
            let bytes = encoded.context("encoding PDU frame")?;
            let encoded_capacity = bytes.capacity();
            if typed.reservation.accounts_retained_bytes() {
                let transition_bytes = typed
                    .reservation
                    .retained_bytes
                    .checked_add(encoded_capacity)
                    .context("counting typed-to-encoded retained allocation transition")?;
                reweight_accounted_batch(
                    terminal,
                    std::slice::from_mut(&mut typed.reservation),
                    transition_bytes,
                )?;
            }
            let ReservedDecodedPdu {
                decoded,
                reservation,
                emission_authority: _,
            } = typed;
            drop(decoded);
            let mut frame = EncodedOutboundFrame {
                bytes,
                reservation,
                authority,
            };
            if frame.reservation.accounts_retained_bytes() {
                reweight_accounted_batch(
                    terminal,
                    std::slice::from_mut(&mut frame.reservation),
                    encoded_capacity,
                )?;
            }
            Ok(frame)
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
    let first = encode_write_payload(first, compression_mode, terminal)?;
    // Declare reservations before bytes so all error paths drop the retained
    // allocation before releasing its accounting authority.
    let mut reservations = vec![first.reservation];
    let mut bytes = first.bytes;
    let mut frames = 1_usize;
    let batch_class = reservations
        .first()
        .map(|reservation| reservation.batch_class)
        .expect("an outbound batch always retains its first reservation");

    while frames < OUTBOUND_WRITE_QUANTUM_FRAMES && bytes.len() < OUTBOUND_WRITE_QUANTUM_BYTES {
        let payload = match item_rx.try_recv() {
            Ok(Item::WritePdu(payload)) => payload,
            Ok(other) => {
                *deferred_item = Some(other);
                break;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        };
        if payload.batch_class() != batch_class {
            // Class boundaries are visible in the reservation before encoding.
            // Defer the typed owner itself so PDU87 never speculatively pays
            // PDU90 serialization before its own flush epoch completes.
            *deferred_item = Some(Item::WritePdu(payload));
            break;
        }
        if reservations
            .iter()
            .any(OutboundReservation::accounts_retained_bytes)
            || payload.accounts_retained_bytes()
        {
            // A retained frame already owns an exact allocation and charge.
            // Concatenating it would allocate and copy before a replacement
            // reservation could be admitted. Keep retained frames segmented
            // until the writer gains an explicitly accounted vectored batch.
            *deferred_item = Some(Item::WritePdu(payload));
            break;
        }
        let frame = encode_write_payload(payload, compression_mode, terminal)?;
        if bytes
            .len()
            .checked_add(frame.bytes.len())
            .is_none_or(|next_len| next_len > OUTBOUND_WRITE_QUANTUM_BYTES)
        {
            *deferred_item = Some(Item::WritePdu(WritePayload::Encoded(frame)));
            break;
        }

        let EncodedOutboundFrame {
            bytes: frame_bytes,
            reservation,
            authority: _,
        } = frame;
        bytes
            .try_reserve(frame_bytes.len())
            .context("reserving unaccounted outbound batch concatenation")?;
        bytes.extend_from_slice(&frame_bytes);
        reservations.push(reservation);
        drop(frame_bytes);
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
        _reservations: reservations,
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
        let Some(admission) = terminal.admit() else {
            return Ok(OutboundService::Terminal);
        };
        drop(admission);
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
        let Some(admission) = terminal.admit() else {
            return Ok(OutboundService::Terminal);
        };
        drop(admission);
        return Ok(OutboundService::Progress);
    }

    // The one-shot write/flush poll registered its precise transport
    // interest but did not suspend this task. Replace it with one combined
    // interest so newly readable input can preempt a blocked outbound window.
    let ready_side = wait_for_dispatch_readable_or_writable(stream, io_uring_runtime)
        .await
        .context("waiting after a pending mux stream write or flush")?;
    let Some(admission) = terminal.admit() else {
        return Ok(OutboundService::Terminal);
    };
    drop(admission);
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

    let terminal = test_terminal();
    let mut current = Some(test_write_payload(first));
    while let Some(payload) = current.take() {
        let EncodedOutboundFrame {
            bytes: frame,
            reservation: _reservation,
            authority: _,
        } = encode_write_payload(payload, compression_mode, &terminal)?;
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

#[cfg(test)]
fn dispatch_client_request(
    handler: &mut SessionHandler,
    topology: &TopologyStreamCoordinator,
    decoded: DecodedPdu,
) -> anyhow::Result<()> {
    dispatch_client_request_with_decode_interval(handler, topology, decoded, None, None)
}

#[derive(Clone, Copy, Debug)]
struct ServerDecodeInterval {
    started_at: std::time::Instant,
    completed_at: std::time::Instant,
}

fn dispatch_client_request_with_decode_interval(
    handler: &mut SessionHandler,
    topology: &TopologyStreamCoordinator,
    decoded: DecodedPdu,
    trace_producer: Option<&Rc<SessionTraceProducer>>,
    decode_interval: Option<ServerDecodeInterval>,
) -> anyhow::Result<()> {
    let result = (|| {
        if decoded.serial == 0 {
            metrics::counter!(
                "mux.dispatch.protocol_error",
                "reason" => "reserved_request_serial_zero"
            )
            .increment(1);
            anyhow::bail!("mux client request used reserved server-unilateral serial zero");
        }

        let wire_spec = decoded
            .pdu
            .wire_spec()
            .context("mux client request has no registered wire identity")?;
        if !wire_spec.authorizes(codec::PduProducer::Client, codec::PduWireRole::Request) {
            anyhow::bail!(
                "mux client sent PDU {} ({}) without client-request direction authority",
                wire_spec.name,
                wire_spec.ident
            );
        }
        if wire_spec.min_codec_version > codec::CODEC_VERSION {
            anyhow::bail!(
                "mux client PDU {} ({}) requires codec dialect {} above local dialect {}",
                wire_spec.name,
                wire_spec.ident,
                wire_spec.min_codec_version,
                codec::CODEC_VERSION
            );
        }

        let mut input_trace_authority = match &decoded.pdu {
            Pdu::SendKeyDownTracedV1(request) => Some(AdmittedInputTraceV1::admit(
                request,
                topology.stream_id,
                codec::CODEC_VERSION,
            )?),
            _ => None,
        };
        if let (Some(admission), Some(interval)) = (input_trace_authority.as_mut(), decode_interval)
        {
            handler.record_decoded_input_trace(
                trace_producer,
                admission,
                interval.started_at,
                interval.completed_at,
            );
        }

        let ordered_authority = match &decoded.pdu {
            Pdu::ListPanesCoherent(request) => {
                topology.begin_fence(decoded.serial, request)?;
                None
            }
            Pdu::ListPanesOrderedV1(request) => {
                topology.begin_ordered_fence(decoded.serial, request)?;
                None
            }
            Pdu::ReorderWindowTabsV1(request) => Some(topology.admit_ordered_reorder(request)?),
            _ => None,
        };
        handler.process_one_with_dispatch_authority(
            decoded,
            ordered_authority,
            input_trace_authority,
        );
        Ok(())
    })();
    if result.is_err() {
        topology.reject_client_request();
    }
    result
}

#[derive(Debug)]
enum ClientDecodeError {
    Decode(anyhow::Error),
    Terminal(&'static str),
    TerminalChannelClosed,
}

/// Keep the newly reserved exact-render family dormant before body admission.
/// The complete ordinary-server dialect/direction/capability authority is
/// tracked separately; this family-specific fence prevents merely adding its
/// codec schema from exposing a large pre-handler allocation surface.
fn select_dormant_client_body(
    header: &codec::PduFrameHeader,
) -> anyhow::Result<codec::PduBodyDisposition> {
    let belongs_to_exact_render =
        Pdu::wire_spec_for_ident(header.ident()).is_some_and(|spec| match spec.capability {
            codec::PduCapabilityUse::Negotiates(capability)
            | codec::PduCapabilityUse::Requires(capability) => {
                capability.contains(TopologyCapabilities::EXACT_RENDER_DELIVERY_V1)
            }
            codec::PduCapabilityUse::None => false,
        });
    if belongs_to_exact_render {
        metrics::counter!(
            "mux.dispatch.protocol_error",
            "reason" => "dormant_exact_render_family",
        )
        .increment(1);
        anyhow::bail!(
            "mux client PDU ident {} belongs to the dormant exact render-delivery family",
            header.ident()
        );
    }
    Ok(codec::PduBodyDisposition::Materialize)
}

async fn decode_client_pdu_or_terminal<T>(
    stream: &mut T,
    terminal_rx: &Receiver<&'static str>,
) -> Result<DecodedPdu, ClientDecodeError>
where
    T: DispatchStream,
{
    let terminal_event = terminal_rx.recv();
    let decode = async {
        match Pdu::decode_async_with_selector(stream, None, select_dormant_client_body).await? {
            codec::AsyncPduDecode::Decoded(decoded) => Ok(decoded),
            codec::AsyncPduDecode::Discarded { ident, serial, .. } => anyhow::bail!(
                "mux server decoder unexpectedly discarded client PDU ident {ident} serial {serial}"
            ),
        }
    };
    pin_mut!(terminal_event);
    pin_mut!(decode);
    match select(terminal_event, decode).await {
        Either::Left((Ok(reason), _)) => Err(ClientDecodeError::Terminal(reason)),
        Either::Left((Err(_), _)) => Err(ClientDecodeError::TerminalChannelClosed),
        Either::Right((result, _)) => result.map_err(ClientDecodeError::Decode),
    }
}

fn admit_request_dispatch(terminal: &DispatchTerminal) -> bool {
    let Some(admission) = terminal.admit() else {
        return false;
    };
    drop(admission);
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestDispatchOutcome {
    Dispatched,
    Terminal,
}

fn pdu_sender_for_topology(topology: &Arc<TopologyStreamCoordinator>) -> PduSender {
    let topology = Arc::downgrade(topology);
    PduSender::new(move |pdu, delivery_class| {
        let topology = topology
            .upgrade()
            .context("mux dispatch connection is retired")?;
        let is_unilateral_render = pdu.serial == 0
            && matches!(
                &pdu.pdu,
                Pdu::GetPaneRenderChangesResponse(_) | Pdu::SetPalette(_) | Pdu::NotifyAlert(_)
            );
        if is_unilateral_render {
            topology.queue_unilateral_render_response(pdu, delivery_class)
        } else {
            topology.queue_response(pdu, delivery_class)
        }
    })
}

struct TopologyNotificationRoute {
    authority: SessionAuthority,
    mux: Weak<Mux>,
    topology: Weak<TopologyStreamCoordinator>,
}

impl TopologyNotificationRoute {
    fn new(
        authority: SessionAuthority,
        mux: &Arc<Mux>,
        topology: &Arc<TopologyStreamCoordinator>,
    ) -> Self {
        Self {
            authority,
            mux: Arc::downgrade(mux),
            topology: Arc::downgrade(topology),
        }
    }

    fn deliver(&self, envelope: MuxNotificationEnvelope) -> bool {
        self.authority
            .try_run(|| {
                let Some(topology) = self.topology.upgrade() else {
                    return false;
                };
                let Some(mux) = self.mux.upgrade() else {
                    return false;
                };
                topology.on_notification(&mux, envelope)
            })
            .unwrap_or(false)
    }
}

fn dispatch_client_request_if_admitted(
    handler: &mut SessionHandler,
    topology: &TopologyStreamCoordinator,
    terminal: &DispatchTerminal,
    decoded: DecodedPdu,
    trace_producer: Option<&Rc<SessionTraceProducer>>,
    decode_interval: Option<ServerDecodeInterval>,
) -> anyhow::Result<RequestDispatchOutcome> {
    if !admit_request_dispatch(terminal) {
        return Ok(RequestDispatchOutcome::Terminal);
    }
    dispatch_client_request_with_decode_interval(
        handler,
        topology,
        decoded,
        trace_producer,
        decode_interval,
    )?;
    Ok(RequestDispatchOutcome::Dispatched)
}

fn retain_pane_alert_or_trip(
    per_pane: &Arc<std::sync::Mutex<PerPane>>,
    terminal: &DispatchTerminal,
    pane_id: mux::pane::PaneId,
    alert: wezterm_term::Alert,
) -> anyhow::Result<()> {
    let retention = match per_pane.lock() {
        Ok(mut state) => match state.push_notification(alert) {
            Ok(()) => Ok(()),
            Err(err) => {
                // Losing an exact alert makes this registration terminal.
                // Close its render authorities under the same mutex guard so
                // no scheduled push can enter between rejection and repair.
                state.retire_render_authority();
                Err(err)
            }
        },
        Err(poison) => {
            retire_poisoned_pane_render(per_pane, poison);
            terminal.trip(PANE_ALERT_BACKLOG_FAILURE);
            anyhow::bail!("per-pane lock poisoned while retaining alert for pane {pane_id}");
        }
    };
    if let Err(err) = retention {
        terminal.trip(PANE_ALERT_BACKLOG_FAILURE);
        metrics::counter!(
            "mux.dispatch.pane_alert_backlog_terminal",
            "reason" => "retention_rejected"
        )
        .increment(1);
        anyhow::bail!("cannot retain mux alert for pane {pane_id}: {err}");
    }
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
    let reactor = DispatchReactor::resolve(config.clone(), stream.dispatch_stream_kind());
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
    let topology_stream_id = TopologyStreamId::from_bytes(*uuid::Uuid::new_v4().as_bytes());
    let topology = Arc::new(TopologyStreamCoordinator::new(
        item_tx.clone(),
        terminal.clone(),
        topology_stream_id,
    ));
    let pdu_sender = pdu_sender_for_topology(&topology);
    let trace_producer = config
        .trace_authority()
        .and_then(|authority| authority.claim_session(topology_stream_id));
    let mut handler =
        SessionHandler::new_for_session_with_topology_stream(pdu_sender, owner, topology_stream_id);

    {
        let notification_route = TopologyNotificationRoute::new(authority.clone(), &mux, &topology);
        let (sub_id, session_incarnation, baseline_revision) = mux
            .subscribe_with_topology_fence(move |envelope| notification_route.deliver(envelope))
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
                    let decode_started_at = std::time::Instant::now();
                    let decoded = match decode_client_pdu_or_terminal(&mut stream, &terminal_rx)
                        .await
                    {
                        Err(ClientDecodeError::Terminal(reason)) => {
                            return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                        }
                        Err(ClientDecodeError::TerminalChannelClosed) => {
                            return Err(anyhow::anyhow!(
                                "mux dispatch terminal channel closed unexpectedly"
                            ));
                        }
                        Err(ClientDecodeError::Decode(err)) => {
                            if !admit_request_dispatch(&terminal) {
                                let reason = terminal_rx
                                    .try_recv()
                                    .ok()
                                    .unwrap_or("mux dispatch connection entered terminal state");
                                return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                            }
                            if is_clean_disconnect(&err) {
                                // Client disconnected: no need to make a noise.
                                return Ok(());
                            }
                            topology.reject_client_request();
                            return Err(err).context("reading Pdu from client");
                        }
                        Ok(data) => data,
                    };
                    let decode_interval = ServerDecodeInterval {
                        started_at: decode_started_at,
                        completed_at: std::time::Instant::now(),
                    };
                    // This short admission is the request-dispatch
                    // linearization point. The wrapper releases it before the
                    // handler because synchronous response callbacks
                    // legitimately re-enter the same terminal admission gate.
                    if dispatch_client_request_if_admitted(
                        &mut handler,
                        &topology,
                        &terminal,
                        decoded,
                        trace_producer.as_ref(),
                        Some(decode_interval),
                    )? == RequestDispatchOutcome::Terminal
                    {
                        let reason = terminal_rx
                            .try_recv()
                            .ok()
                            .unwrap_or("mux dispatch connection entered terminal state");
                        return Err(anyhow::anyhow!("mux dispatch terminated: {reason}"));
                    }
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
                        MuxNotification::FloatingPaneSpawnCommitted(spawn) => {
                            // Legacy delivery has no atomic floating-spawn PDU.
                            // Trigger one authoritative snapshot resync; the
                            // compact stamped stream event handles fenced peers.
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabResized(codec::TabResized {
                                    tab_id: spawn.tab_id(),
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
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
                                retain_pane_alert_or_trip(&per_pane, &terminal, pane_id, alert)?;
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
                        MuxNotification::WindowOrderChanged { window, .. } => {
                            // PDU90 is intentionally dormant. `TabResized` is
                            // the established legacy resync trigger and its
                            // client handler deliberately ignores the id.
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabResized(codec::TabResized {
                                    tab_id: window.active_tab_id().unwrap_or(0),
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
                        MuxNotification::WindowTopologyChanged(change) => {
                            pending_outbound = Some(prepare_unilateral_pdu(
                                Pdu::TabResized(codec::TabResized {
                                    tab_id: change.legacy_resync_tab_id(),
                                }),
                                reservation,
                                &item_rx,
                                &mut deferred_item,
                                &terminal,
                            )?);
                        }
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
    use codec::{
        CompressionMode, InputSerial, Ping, Pong, SampledTraceContextV1, SendKeyDown,
        SendKeyDownTracedV1, WriteToPane,
    };
    use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
        RecorderEpochId, RecorderSamplerAlgorithm, SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
    };
    use frankenterm_core_audit_types::interaction_trace_v2::{
        InteractionTraceId, InteractionTracePath, InteractionTraceRunId,
    };
    use mux::domain::DomainId;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, WithPaneLines};
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use parking_lot::{MappedMutexGuard, Mutex as ParkingMutex, MutexGuard as ParkingMutexGuard};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::io;
    use std::io::Cursor;

    fn sampled_key_request() -> SendKeyDownTracedV1 {
        SendKeyDownTracedV1 {
            request: SendKeyDown {
                pane_id: 7_007,
                event: termwiz::input::KeyEvent {
                    key: termwiz::input::KeyCode::Char('x'),
                    modifiers: termwiz::input::Modifiers::NONE,
                },
                input_serial: InputSerial::from_millis_since_epoch(11),
            },
            trace_context: SampledTraceContextV1 {
                schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
                trace_id: InteractionTraceId {
                    run_id: InteractionTraceRunId {
                        epoch_nonce_hi: 0x8182_8384_8586_8788,
                        epoch_nonce_lo: 0x9192_9394_9596_9798,
                    },
                    sequence: 29,
                },
                path: InteractionTracePath::Keypress,
                origin_recorder_epoch_id: RecorderEpochId {
                    nonce_hi: 0xa1a2_a3a4_a5a6_a7a8,
                    nonce_lo: 0xb1b2_b3b4_b5b6_b7b8,
                },
                sampler_algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            },
        }
    }
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
        let sender = PduSender::new(move |pdu, _class| {
            captured_for_sender.lock().push(pdu);
            Ok(())
        });
        (sender, captured)
    }

    fn idle_topology_coordinator() -> TopologyStreamCoordinator {
        let (item_tx, _item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, _terminal_rx) = DispatchTerminal::channel();
        TopologyStreamCoordinator::new(item_tx, terminal, TopologyStreamId::from_bytes([0x5a; 16]))
    }

    #[test]
    fn pane_alert_retention_rejection_trips_the_affected_dispatch_terminal() {
        let per_pane = Arc::new(std::sync::Mutex::new(PerPane::default()));
        {
            let mut state = per_pane.lock().unwrap();
            for _ in 0..(codec::MAX_RENDER_APPLICATION_ALERTS * 2) {
                state
                    .push_notification(Alert::Bell)
                    .expect("fill the bounded exact-event backlog");
            }
        }
        let (terminal, terminal_rx) = DispatchTerminal::channel();

        let error = retain_pane_alert_or_trip(&per_pane, &terminal, 91, Alert::Bell)
            .expect_err("an unretainable exact event must fail the connection closed");

        assert!(
            error
                .to_string()
                .contains("exact-event alert backlog capacity"),
            "typed retention failure should survive dispatch propagation: {error:#}"
        );
        assert!(terminal.is_tripped());
        assert_eq!(
            terminal_rx.try_recv().expect("terminal reason"),
            PANE_ALERT_BACKLOG_FAILURE
        );
        assert_eq!(
            per_pane.lock().unwrap().notifications.len(),
            codec::MAX_RENDER_APPLICATION_ALERTS * 2,
            "rejection must not mutate the exact retained prefix"
        );
    }

    #[test]
    fn poisoned_pane_alert_retention_repairs_state_before_returning() {
        let per_pane = Arc::new(std::sync::Mutex::new(PerPane::default()));
        let poison_target = Arc::clone(&per_pane);
        assert!(
            std::thread::spawn(move || {
                let _held = poison_target
                    .lock()
                    .expect("test pane state starts unpoisoned");
                panic!("synthetic pane-alert state poison");
            })
            .join()
            .is_err()
        );
        let (terminal, terminal_rx) = DispatchTerminal::channel();

        let error = retain_pane_alert_or_trip(&per_pane, &terminal, 92, Alert::Bell)
            .expect_err("poisoned alert retention must fail the connection closed");

        assert!(error.to_string().contains("per-pane lock poisoned"));
        let repaired = per_pane
            .lock()
            .expect("poison repair must complete before retention returns");
        assert!(repaired.notifications.is_empty());
        assert_eq!(
            terminal_rx.try_recv().expect("terminal reason"),
            PANE_ALERT_BACKLOG_FAILURE
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
    }

    fn bound_topology_coordinator() -> (
        TopologyStreamCoordinator,
        Receiver<Item>,
        Receiver<&'static str>,
        MuxSessionIncarnation,
        TopologyStreamId,
    ) {
        bound_topology_coordinator_with_item_capacity(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY)
    }

    fn bound_topology_coordinator_with_item_capacity(
        item_capacity: usize,
    ) -> (
        TopologyStreamCoordinator,
        Receiver<Item>,
        Receiver<&'static str>,
        MuxSessionIncarnation,
        TopologyStreamId,
    ) {
        let (item_tx, item_rx) = bounded(item_capacity);
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
                    floating_panes: Vec::new(),
                },
            }),
        }
    }

    fn coherent_snapshot_response_with_tab_titles(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: TopologyRevision,
        tab_titles: Vec<String>,
    ) -> ListPanesCoherentResponse {
        let mut response =
            coherent_snapshot_response(stream_id, session_incarnation, snapshot_revision);
        let ListPanesCoherentOutcome::Snapshot(snapshot) = &mut response.outcome else {
            unreachable!("coherent response helper always returns a snapshot");
        };
        snapshot.panes.tabs = vec![mux::tab::PaneNode::Empty; tab_titles.len()];
        snapshot.panes.tab_titles = tab_titles;
        response
    }

    fn ordered_window_capabilities(include_reorder: bool) -> TopologyCapabilities {
        let mut bits = ordered_snapshot_foundation().bits();
        if include_reorder {
            bits |= TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits();
        }
        TopologyCapabilities::from_bits(bits)
    }

    fn ordered_snapshot_request(include_reorder: bool) -> codec::ListPanesOrderedV1 {
        let capabilities = ordered_window_capabilities(include_reorder);
        codec::ListPanesOrderedV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: codec::DomainBindingId::from_bytes([0xd1; 16]),
            supported: capabilities,
            required: capabilities,
        }
    }

    fn ordered_snapshot_response(
        request: &codec::ListPanesOrderedV1,
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
        snapshot_revision: TopologyRevision,
    ) -> codec::ListPanesOrderedV1Response {
        codec::ListPanesOrderedV1Response {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: request.domain_binding_id,
            negotiated: request.supported,
            stream_id,
            outcome: codec::ListPanesOrderedV1Outcome::Snapshot(codec::OrderedPaneSnapshotV1 {
                session_incarnation,
                topology_revision: snapshot_revision,
                panes: codec::ordered_pane_arena_from_list_panes(codec::ListPanesResponse {
                    tabs: Vec::new(),
                    tab_titles: Vec::new(),
                    window_titles: std::collections::HashMap::new(),
                    floating_panes: Vec::new(),
                })
                .expect("empty ordered-pane arena must be valid"),
                floating_panes: Vec::new(),
                ordered_windows: Vec::new(),
            }),
        }
    }

    fn ordered_unsupported_response(
        request: &codec::ListPanesOrderedV1,
        stream_id: TopologyStreamId,
        server_supported: TopologyCapabilities,
    ) -> codec::ListPanesOrderedV1Response {
        codec::ListPanesOrderedV1Response {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: request.domain_binding_id,
            negotiated: request.supported.intersection(server_supported),
            stream_id,
            outcome: codec::ListPanesOrderedV1Outcome::Unsupported {
                supported: server_supported,
            },
        }
    }

    fn ordered_reorder_request(
        request: &codec::ListPanesOrderedV1,
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
    ) -> codec::ReorderWindowTabsV1 {
        codec::ReorderWindowTabsV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: request.domain_binding_id,
            stream_id,
            session_incarnation,
            window_id: codec::RemoteWindowId::new(1),
            expected_order_revision: codec::WindowOrderRevision::INITIAL,
            desired_tab_ids: Vec::new(),
            desired_active_tab_id: None,
            mutation_id: codec::WindowOrderMutationId::new([0x71; 16], 1),
            digest: codec::WindowReorderDigest::ZERO,
        }
        .with_computed_digest()
    }

    fn dormant_reorder_response(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
    ) -> Pdu {
        Pdu::ReorderWindowTabsV1Response(codec::ReorderWindowTabsV1Response {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            stream_id,
            session_incarnation,
            mutation_id: codec::WindowOrderMutationId::new([0x72; 16], 1),
            request_digest: codec::WindowReorderDigest::from_bytes([0x73; 32]),
            outcome: codec::ReorderWindowTabsV1Outcome::Malformed,
        })
    }

    fn topology_envelope(revision: u64, notification: MuxNotification) -> MuxNotificationEnvelope {
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
        try_retained_topology_event(
            prepare_retained_topology_notification(notification)
                .expect("test topology notification should prepare"),
            revision,
            &coordinator.terminal,
            &coordinator.outbound_budget,
        )
        .expect("test topology notification should fit its connection budget")
    }

    #[test]
    fn retained_topology_admission_rejects_unprepared_frozen_window_graph() {
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for frozen ordered state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");
        let prepared = PreparedTopologyNotification {
            notification: RetainedTopologyNotification::Ordinary(
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0x61; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0x62; 32]),
                    window: frozen,
                },
            ),
            dynamic_bytes: 0,
        };
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let budget = Arc::new(OutboundBudget::default());

        let rejected =
            try_retained_topology_event(prepared, TopologyRevision::INITIAL, &terminal, &budget)
                .expect_err("an unprepared frozen mux graph must never gain retention authority");

        assert!(
            format!("{:#}", rejected.error).contains(
                "unprepared frozen window-order graph reached retained topology admission"
            )
        );
        assert_eq!(rejected.notification.dynamic_bytes, 0);
        assert!(matches!(
            rejected.notification.notification,
            RetainedTopologyNotification::Ordinary(MuxNotification::WindowOrderChanged {
                ref window,
                ..
            }) if window.window_id() == window_id
        ));
        assert_outbound_budget_live_counters_zero(&budget);
        assert!(
            terminal_rx.is_empty(),
            "preflight rejection must return the owner before admission without changing terminal state"
        );
    }

    #[test]
    fn frozen_window_transaction_reaches_legacy_client_as_one_resync() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register frozen-transaction test tab");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        let captured = Arc::new(ParkingMutex::new(None));
        let captured_for_subscriber = Arc::clone(&captured);
        mux.subscribe_with_topology(move |envelope| {
            if matches!(
                &envelope.notification,
                MuxNotification::WindowTopologyChanged(_)
            ) {
                *captured_for_subscriber.lock() = Some(envelope);
            }
            true
        })
        .expect("subscribe to exact frozen transaction");

        mux.add_tab_to_window(&tab, window_id)
            .expect("publish one created-and-attached window transaction");
        let envelope = captured
            .lock()
            .take()
            .expect("mux must publish the frozen transaction");
        let MuxTopologyStamp::Revision(_) = envelope.topology else {
            panic!("frozen transaction must carry live topology authority");
        };
        assert!(coordinator.on_notification(&mux, envelope));

        let emitted = take_written_pdu(&item_rx);
        assert_eq!(emitted.serial, 0);
        let Pdu::TabResized(resync) = emitted.pdu else {
            panic!("legacy stream must receive one established resync trigger");
        };
        assert_eq!(resync.tab_id, tab.tab_id());
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);

        drop(window);
    }

    fn assert_outbound_budget_live_counters_zero(budget: &OutboundBudget) {
        let released = budget.snapshot();
        assert_eq!(
            released.retained_bytes, 0,
            "terminal topology cleanup must release every live retained byte"
        );
        assert_eq!(
            released.topology_retained_bytes, 0,
            "terminal topology cleanup must release the topology tranche"
        );
        assert_eq!(
            released.snapshot_retained_bytes, 0,
            "terminal topology cleanup must release the ordered-snapshot tranche"
        );
        assert_eq!(
            released.total_slots, 0,
            "terminal topology cleanup must release every live outbound slot"
        );
        assert_eq!(
            released.bulk_slots, 0,
            "terminal topology cleanup must release every live bulk/topology slot"
        );
    }

    fn assert_outbound_live_counters_zero(coordinator: &TopologyStreamCoordinator) {
        assert_outbound_budget_live_counters_zero(&coordinator.outbound_budget);
        let released = coordinator.outbound_budget.snapshot();
        assert!(
            released.peak_retained_bytes <= OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES,
            "the historical topology high-water mark must remain within the connection cap"
        );
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

        for (revision, (notification, dynamic_capacity, expected_event)) in (1_u64..).zip(cases) {
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
            .insert(retained, TopologyRetentionLimits::default())
            .expect("one roughly 2.1 MiB title should fit the retained-byte budget");
        let retained = buffer
            .remove(TopologyRevision::new(4))
            .expect("removing retained title should maintain byte accounting")
            .expect("large title should remain buffered");
        assert_eq!(buffer.retained_bytes, 0);
        assert!(buffer.events.is_empty());

        coordinator
            .queue_stamped_event(None, retained)
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

    #[test]
    fn topology_budget_survives_fence_restore_at_operating_queue_depths() {
        for depth in [2_usize, 20, 200, DISPATCH_ITEM_QUEUE_CAPACITY] {
            let (coordinator, item_rx, terminal_rx, _session_incarnation, stream_id) =
                bound_topology_coordinator();
            let mux = Mux::new(None);
            let serial = 10_000_u64
                .checked_add(u64::try_from(depth).expect("test depth fits u64"))
                .expect("test serial stays in range");
            coordinator
                .begin_fence(serial, &fenced_snapshot_request())
                .expect("begin queue-depth topology fence");

            for revision in 1..=depth {
                assert!(coordinator.on_notification(
                    &mux,
                    topology_envelope(
                        u64::try_from(revision).expect("test revision fits u64"),
                        MuxNotification::PaneAdded(revision),
                    ),
                ));
            }

            let retained = coordinator.outbound_budget.snapshot();
            assert_eq!(retained.total_slots, depth);
            assert_eq!(retained.bulk_slots, depth);
            assert_eq!(
                retained.retained_bytes,
                RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES
                    .checked_mul(depth)
                    .expect("test retained-byte total fits usize")
            );
            assert!(retained.retained_bytes <= OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES);

            coordinator
                .queue_response(
                    DecodedPdu {
                        serial,
                        pdu: Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                            stream_id,
                            outcome: ListPanesCoherentOutcome::Contended {
                                attempts: 1,
                                first_revision: TopologyRevision::INITIAL,
                                last_revision: TopologyRevision::new(
                                    u64::try_from(depth).expect("test depth fits u64"),
                                ),
                            },
                        }),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("control response must use reserved headroom at every bulk depth");

            let queued = coordinator.outbound_budget.snapshot();
            assert_eq!(queued.total_slots, depth + 1);
            assert_eq!(queued.bulk_slots, depth);
            assert_eq!(queued.retained_bytes, retained.retained_bytes);

            let response = take_written_pdu(&item_rx);
            assert_eq!(response.serial, serial);
            assert!(matches!(
                response.pdu,
                Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                    outcome: ListPanesCoherentOutcome::Contended { .. },
                    ..
                })
            ));
            for expected_pane_id in 1..=depth {
                assert!(matches!(
                    item_rx.try_recv().expect("restored topology notification"),
                    Item::Notif(ReservedNotification {
                        notification: MuxNotification::PaneAdded(pane_id),
                        ..
                    }) if pane_id == expected_pane_id
                ));
            }
            assert!(item_rx.is_empty());
            assert!(terminal_rx.is_empty());
            let released = coordinator.outbound_budget.snapshot();
            assert_eq!(released.retained_bytes, 0);
            assert_eq!(released.total_slots, 0);
            assert_eq!(released.bulk_slots, 0);
            assert!(released.peak_retained_bytes <= OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES);
        }
    }

    #[test]
    fn established_topology_budget_survives_partial_write_and_flush_at_queue_depths() {
        for depth in [2_usize, 20, 200, DISPATCH_ITEM_QUEUE_CAPACITY] {
            let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
                bound_topology_coordinator();
            let mux = Mux::new(None);
            let serial = 20_000_u64
                .checked_add(u64::try_from(depth).expect("test depth fits u64"))
                .expect("test serial stays in range");
            coordinator
                .begin_fence(serial, &fenced_snapshot_request())
                .expect("begin established queue-depth fence");
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial,
                        pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                            stream_id,
                            session_incarnation,
                            TopologyRevision::INITIAL,
                        )),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("establish queue-depth topology stream");
            let snapshot = take_written_pdu(&item_rx);
            assert_eq!(snapshot.serial, serial);

            for revision in 1..=depth {
                assert!(coordinator.on_notification(
                    &mux,
                    topology_envelope(
                        u64::try_from(revision).expect("test revision fits u64"),
                        MuxNotification::PaneAdded(revision),
                    ),
                ));
            }
            let queued = coordinator.outbound_budget.snapshot();
            assert_eq!(queued.total_slots, depth);
            assert_eq!(queued.bulk_slots, depth);
            assert_eq!(
                queued.retained_bytes,
                RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES
                    .checked_mul(depth)
                    .expect("test retained-byte total fits usize")
            );

            let wire = drain_queued_write_pdus(&item_rx, &coordinator.terminal);
            let mut cursor = Cursor::new(wire.as_slice());
            for expected_revision in 1..=depth {
                let decoded = Pdu::decode(&mut cursor).expect("decode stamped topology frame");
                assert_eq!(decoded.serial, 0);
                let Pdu::TopologyEvent(event) = decoded.pdu else {
                    panic!("established topology wire frame must be stamped");
                };
                assert_eq!(
                    event.revision,
                    TopologyRevision::new(
                        u64::try_from(expected_revision).expect("test revision fits u64")
                    )
                );
            }
            assert_eq!(cursor.position() as usize, wire.len());
            assert!(item_rx.is_empty());
            assert!(terminal_rx.is_empty());
            let released = coordinator.outbound_budget.snapshot();
            assert_eq!(released.retained_bytes, 0);
            assert_eq!(released.total_slots, 0);
            assert_eq!(released.bulk_slots, 0);
            assert!(released.peak_retained_bytes <= OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES);
        }
    }

    #[test]
    fn bulk_saturation_preserves_all_control_reserve_slots_and_wire_order() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        for _ in 0..DISPATCH_ITEM_QUEUE_CAPACITY {
            coordinator
                .queue_response(
                    DecodedPdu {
                        pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 17 }),
                        serial: 0,
                    },
                    PduDeliveryClass::Bulk,
                )
                .expect("bulk response should fill only the bulk partition");
        }
        let saturated = coordinator.outbound_budget.snapshot();
        assert_eq!(saturated.bulk_slots, DISPATCH_ITEM_QUEUE_CAPACITY);
        assert_eq!(saturated.total_slots, DISPATCH_ITEM_QUEUE_CAPACITY);

        for serial in 1..=DISPATCH_ITEM_QUEUE_CONTROL_RESERVE {
            coordinator
                .queue_response(
                    DecodedPdu {
                        pdu: Pdu::Pong(Pong {}),
                        serial: u64::try_from(serial).expect("control serial fits u64"),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("every reserved control slot must remain available");
        }
        assert_eq!(
            coordinator.outbound_budget.snapshot().total_slots,
            DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY
        );

        let wire = drain_queued_write_pdus(&item_rx, &coordinator.terminal);
        let mut cursor = Cursor::new(wire.as_slice());
        for _ in 0..DISPATCH_ITEM_QUEUE_CAPACITY {
            let decoded = Pdu::decode(&mut cursor).expect("decode bulk wire predecessor");
            assert_eq!(decoded.serial, 0);
            assert_eq!(
                decoded.pdu,
                Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 17 })
            );
        }
        for expected_serial in 1..=DISPATCH_ITEM_QUEUE_CONTROL_RESERVE {
            let decoded = Pdu::decode(&mut cursor).expect("decode reserved control response");
            assert_eq!(
                decoded.serial,
                u64::try_from(expected_serial).expect("control serial fits u64")
            );
            assert_eq!(decoded.pdu, Pdu::Pong(Pong {}));
        }
        assert_eq!(cursor.position() as usize, wire.len());
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        let released = coordinator.outbound_budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn sixty_fifth_control_slot_is_the_first_terminal_overflow_and_releases_on_teardown() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        for _ in 0..DISPATCH_ITEM_QUEUE_CAPACITY {
            coordinator
                .queue_response(
                    DecodedPdu {
                        pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 23 }),
                        serial: 0,
                    },
                    PduDeliveryClass::Bulk,
                )
                .expect("fill bulk partition");
        }
        for serial in 1..=DISPATCH_ITEM_QUEUE_CONTROL_RESERVE {
            coordinator
                .queue_response(
                    DecodedPdu {
                        pdu: Pdu::Pong(Pong {}),
                        serial: u64::try_from(serial).expect("control serial fits u64"),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("fill reserved control partition");
        }

        let queued_before_overflow = item_rx.len();
        coordinator
            .queue_response(
                DecodedPdu {
                    pdu: Pdu::Pong(Pong {}),
                    serial: 65,
                },
                PduDeliveryClass::Control,
            )
            .expect_err("the first item beyond total capacity must fail closed");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("first total-slot overflow must publish its reason"),
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(terminal_rx.is_empty());
        assert_eq!(item_rx.len(), queued_before_overflow);
        assert!(
            coordinator
                .queue_response(
                    DecodedPdu {
                        pdu: Pdu::Pong(Pong {}),
                        serial: 66,
                    },
                    PduDeliveryClass::Control,
                )
                .is_err(),
            "no response may publish after the first terminal overflow"
        );
        assert_eq!(item_rx.len(), queued_before_overflow);

        while item_rx.try_recv().is_ok() {}
        let released = coordinator.outbound_budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn queue_teardown_and_reconnect_release_generation_local_budget() {
        let (first, first_rx, first_terminal_rx, _, _) = bound_topology_coordinator();
        let first_budget = Arc::clone(&first.outbound_budget);
        let mux = Mux::new(None);
        for revision in 1..=20 {
            assert!(first.on_notification(
                &mux,
                topology_envelope(
                    revision,
                    MuxNotification::PaneAdded(
                        usize::try_from(revision).expect("test revision fits usize"),
                    ),
                ),
            ));
        }
        let first_queued = first.outbound_budget.snapshot();
        assert_eq!(first_queued.total_slots, 20);
        assert_eq!(first_queued.bulk_slots, 20);
        assert!(first_queued.retained_bytes > 0);

        drop(first_rx);
        assert_eq!(
            first.outbound_budget.snapshot(),
            first_queued,
            "closing the receiver must retain already-queued owners until channel teardown"
        );
        assert!(
            !first.on_notification(&mux, topology_envelope(21, MuxNotification::PaneAdded(21)),)
        );
        assert_eq!(
            first_terminal_rx
                .try_recv()
                .expect("closed queue must terminate its connection generation"),
            NOTIFICATION_QUEUE_CLOSED
        );
        let after_rejected = first.outbound_budget.snapshot();
        assert_eq!(
            after_rejected.retained_bytes, first_queued.retained_bytes,
            "the rejected closed-channel item must release only its own retained bytes"
        );
        assert_eq!(
            after_rejected.total_slots, first_queued.total_slots,
            "the rejected closed-channel item must release only its own total slot"
        );
        assert_eq!(
            after_rejected.bulk_slots, first_queued.bulk_slots,
            "the rejected closed-channel topology item must not perturb bulk slots"
        );
        assert_eq!(
            after_rejected.peak_retained_bytes,
            first_queued
                .peak_retained_bytes
                .checked_add(RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES)
                .expect("test topology high-water fits usize"),
            "the high-water mark must retain the transient rejected reservation"
        );
        drop(first);
        let first_released = first_budget.snapshot();
        assert_eq!(first_released.retained_bytes, 0);
        assert_eq!(first_released.total_slots, 0);
        assert_eq!(first_released.bulk_slots, 0);

        let (second, second_rx, second_terminal_rx, _, _) = bound_topology_coordinator();
        let second_budget = Arc::clone(&second.outbound_budget);
        assert_eq!(
            second.outbound_budget.snapshot(),
            OutboundBudgetState::default()
        );
        assert!(second.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),));
        let second_queued = second.outbound_budget.snapshot();
        assert_eq!(second_queued.total_slots, 1);
        assert_eq!(second_queued.bulk_slots, 1);
        assert!(second_queued.retained_bytes > 0);
        drop(second_rx);
        drop(second);
        let second_released = second_budget.snapshot();
        assert_eq!(second_released.retained_bytes, 0);
        assert_eq!(second_released.total_slots, 0);
        assert_eq!(second_released.bulk_slots, 0);
        assert!(second_terminal_rx.is_empty());
    }

    #[test]
    fn lingering_producer_callbacks_do_not_retain_dispatch_connection_or_budget() {
        let (coordinator, item_rx, _terminal_rx, _, _) = bound_topology_coordinator();
        let coordinator = Arc::new(coordinator);
        let budget = Arc::clone(&coordinator.outbound_budget);
        let weak_coordinator = Arc::downgrade(&coordinator);
        let sender = pdu_sender_for_topology(&coordinator);

        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let owner = SessionOwner::new(Arc::clone(&mux));
        let notification_route =
            TopologyNotificationRoute::new(owner.authority(), &mux, &coordinator);

        sender
            .send_bulk(DecodedPdu {
                pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 29 }),
                serial: 0,
            })
            .expect("queue one response through the production weak sender");
        let queued = budget.snapshot();
        assert_eq!(queued.total_slots, 1);
        assert_eq!(queued.bulk_slots, 1);

        drop(item_rx);
        assert_eq!(
            budget.snapshot(),
            queued,
            "a live coordinator sender must retain its closed channel queue"
        );
        drop(coordinator);
        assert!(weak_coordinator.upgrade().is_none());
        assert_eq!(budget.snapshot(), OutboundBudgetState::default());

        drop(owner);
        drop(mux);
        assert!(weak_mux.upgrade().is_none());
        let error = sender
            .send_control(DecodedPdu {
                pdu: Pdu::Pong(Pong {}),
                serial: 1,
            })
            .expect_err("a lingering response callback must fail after connection teardown");
        assert!(format!("{error:#}").contains("mux dispatch connection is retired"));
        let request = ordered_snapshot_request(true);
        let error = sender
            .send_control(DecodedPdu {
                pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                    &request,
                    TopologyStreamId::from_bytes([0x5a; 16]),
                    MuxSessionIncarnation::from_bytes([0xa5; 16]),
                    TopologyRevision::INITIAL,
                )),
                serial: 2,
            })
            .expect_err("a pending PDU87 callback must fail after coordinator teardown");
        assert!(format!("{error:#}").contains("mux dispatch connection is retired"));
        assert_eq!(budget.snapshot(), OutboundBudgetState::default());
        assert!(!notification_route.deliver(topology_envelope(1, MuxNotification::PaneAdded(1),)));
    }

    #[test]
    fn unilateral_render_sender_preserves_success_after_post_publish_terminal_trip() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        let coordinator = Arc::new(coordinator);
        coordinator.set_after_unilateral_render_publish_hook({
            let terminal = coordinator.terminal.clone();
            move || terminal.trip(TOPOLOGY_PROTOCOL_FAILURE)
        });
        let sender = pdu_sender_for_topology(&coordinator);

        sender
            .send_bulk(DecodedPdu {
                pdu: Pdu::NotifyAlert(codec::NotifyAlert {
                    pane_id: 91,
                    alert: Alert::Bell,
                }),
                serial: 0,
            })
            .expect(
                "publication is authoritative even when the connection becomes terminal immediately afterwards",
            );

        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("the post-publication hook must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        let queued = take_written_pdu(&item_rx);
        assert_eq!(queued.serial, 0);
        assert!(matches!(
            queued.pdu,
            Pdu::NotifyAlert(codec::NotifyAlert {
                pane_id: 91,
                alert: Alert::Bell,
            })
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn unilateral_alert_rejects_control_class_before_publication() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        let coordinator = Arc::new(coordinator);
        let sender = pdu_sender_for_topology(&coordinator);

        let error = sender
            .send_control(DecodedPdu {
                pdu: Pdu::NotifyAlert(codec::NotifyAlert {
                    pane_id: 91,
                    alert: Alert::Bell,
                }),
                serial: 0,
            })
            .expect_err("unilateral alerts must use the bounded bulk class");

        assert!(error.to_string().contains("invalid PDU or delivery class"));
        assert!(item_rx.is_empty(), "an invalid class must not publish");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("invalid unilateral admission must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_response_preserves_success_after_post_publish_terminal_trip() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                198,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        coordinator.set_after_ordered_snapshot_publish_hook({
            let terminal = coordinator.terminal.clone();
            move || terminal.trip(TOPOLOGY_PROTOCOL_FAILURE)
        });

        coordinator
            .queue_response(
                DecodedPdu {
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                    serial: 198,
                },
                PduDeliveryClass::Control,
            )
            .expect(
                "ordered publication is authoritative even when the connection becomes terminal immediately afterwards",
            );

        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("the post-publication hook must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        let queued = take_written_pdu(&item_rx);
        assert_eq!(queued.serial, 198);
        assert!(matches!(queued.pdu, Pdu::ListPanesOrderedV1Response(_)));
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn coherent_response_preserves_success_after_buffered_event_queue_loss() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        coordinator
            .begin_fence(199, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        let mux = Mux::new(None);
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                    serial: 199,
                },
                PduDeliveryClass::Control,
            )
            .expect("published PDU86 must remain admitted when its buffered event cannot queue");

        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("buffered event queue loss must terminate the connection"),
            NOTIFICATION_QUEUE_OVERFLOW
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        let queued = take_written_pdu(&item_rx);
        assert_eq!(queued.serial, 199);
        assert!(matches!(queued.pdu, Pdu::ListPanesCoherentResponse(_)));
        assert!(item_rx.is_empty(), "failed topology event must not publish");
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn generic_response_preserves_success_after_fence_restore_queue_loss() {
        let (coordinator, item_rx, terminal_rx, _, _) =
            bound_topology_coordinator_with_item_capacity(1);
        coordinator
            .begin_fence(200, &fenced_snapshot_request())
            .expect("begin coherent topology fence");
        let mux = Mux::new(None);
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    pdu: Pdu::Pong(Pong {}),
                    serial: 200,
                },
                PduDeliveryClass::Control,
            )
            .expect(
                "published generic response must remain admitted when fence restore cannot queue",
            );

        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("fence-restore queue loss must terminate the connection"),
            NOTIFICATION_QUEUE_OVERFLOW
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        let queued = take_written_pdu(&item_rx);
        assert_eq!(queued.serial, 200);
        assert!(matches!(queued.pdu, Pdu::Pong(Pong {})));
        assert!(item_rx.is_empty(), "failed restored event must not publish");
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn reconnect_rejects_delayed_old_stream_callback_without_touching_new_stream() {
        let mux = Arc::new(Mux::new(None));
        let (old_tx, old_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (old_terminal, old_terminal_rx) = DispatchTerminal::channel();
        let old = Arc::new(TopologyStreamCoordinator::new(
            old_tx,
            old_terminal,
            TopologyStreamId::from_bytes([0x41; 16]),
        ));
        old.bind_subscription(
            MuxSessionIncarnation::from_bytes([0x51; 16]),
            TopologyRevision::INITIAL,
        )
        .expect("bind old connection generation");
        let old_owner = SessionOwner::new(Arc::clone(&mux));
        let old_route = TopologyNotificationRoute::new(old_owner.authority(), &mux, &old);

        let (new_tx, new_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (new_terminal, new_terminal_rx) = DispatchTerminal::channel();
        let new = Arc::new(TopologyStreamCoordinator::new(
            new_tx,
            new_terminal,
            TopologyStreamId::from_bytes([0x42; 16]),
        ));
        new.bind_subscription(
            MuxSessionIncarnation::from_bytes([0x52; 16]),
            TopologyRevision::INITIAL,
        )
        .expect("bind replacement connection generation");
        let new_owner = SessionOwner::new(Arc::clone(&mux));
        let new_route = TopologyNotificationRoute::new(new_owner.authority(), &mux, &new);

        drop(old_owner);
        assert!(
            !old_route.deliver(topology_envelope(1, MuxNotification::PaneAdded(1))),
            "retired connection authority must reject a delayed old-stream callback"
        );
        assert!(old_rx.is_empty());
        assert_outbound_live_counters_zero(&old);

        assert!(new_route.deliver(topology_envelope(1, MuxNotification::PaneAdded(2),)));
        let Item::Notif(ReservedNotification {
            notification: MuxNotification::PaneAdded(2),
            ..
        }) = new_rx
            .try_recv()
            .expect("replacement route must retain its own callback")
        else {
            panic!("replacement stream must receive only its own notification")
        };
        assert!(new_rx.is_empty());
        assert!(old_terminal_rx.is_empty());
        assert!(new_terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&new);
        drop(new_owner);
    }

    #[test]
    fn stamped_topology_charge_reweights_to_pending_allocation_and_releases() {
        let (coordinator, item_rx, _terminal_rx, _, _) = bound_topology_coordinator();
        let mut title = String::with_capacity(16 * 1024);
        title.push_str("short-title");
        let retained = retain_topology_for_test(
            &coordinator,
            MuxNotification::WindowTitleChanged {
                window_id: 31,
                title,
            },
            TopologyRevision::new(7),
        );
        coordinator
            .queue_stamped_event(None, retained)
            .expect("queue stamped topology event");
        let payload = match item_rx.try_recv().expect("queued stamped topology event") {
            Item::WritePdu(payload) => payload,
            other => panic!("expected queued topology PDU, got {other:?}"),
        };
        let mut deferred_item = None;
        let pending = prepare_pending_outbound_batch(
            payload,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &coordinator.terminal,
        )
        .expect("encode retained topology event once");
        assert!(deferred_item.is_none());
        let encoded = coordinator.outbound_budget.snapshot();
        assert_eq!(encoded.total_slots, 1);
        assert_eq!(encoded.bulk_slots, 1);
        assert_eq!(encoded.retained_bytes, pending.bytes.capacity());
        assert!(encoded.retained_bytes <= OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES);
        drop(pending);
        let released = coordinator.outbound_budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn typed_to_encoded_reweight_overflow_releases_failed_frame_before_authority() {
        let budget = Arc::new(OutboundBudget::default());
        let other = budget
            .try_reserve(
                OutboundClass::Topology,
                TOPOLOGY_FENCE_MAX_RETAINED_BYTES - 2,
            )
            .expect("reserve near-ceiling unrelated accounted bytes");
        let reservation = budget
            .try_reserve(OutboundClass::Topology, 1)
            .expect("reserve one typed topology byte");
        let (terminal, terminal_rx) = DispatchTerminal::channel();

        let error = encode_write_payload(
            WritePayload::Typed(ReservedDecodedPdu {
                decoded: queued_pong(1),
                reservation,
                emission_authority: ServerEmissionAuthority::Ordinary,
            }),
            CompressionMode::Never,
            &terminal,
        )
        .expect_err("typed and encoded overlap must not exceed the retained-byte ceiling");
        assert!(format!("{error:#}").contains("retained_bytes"));
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("reweight overflow must terminate the connection"),
            OUTBOUND_BUDGET_OVERFLOW
        );
        let failed_released = budget.snapshot();
        assert_eq!(
            failed_released.retained_bytes,
            TOPOLOGY_FENCE_MAX_RETAINED_BYTES - 2
        );
        assert_eq!(failed_released.total_slots, 1);
        assert_eq!(failed_released.bulk_slots, 1);
        drop(other);
        let released = budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn topology_and_snapshot_frame_retained_tranches_cannot_borrow() {
        let budget = Arc::new(OutboundBudget::default());
        let topology = budget
            .try_reserve(OutboundClass::Topology, TOPOLOGY_FENCE_MAX_RETAINED_BYTES)
            .expect("the exact topology tranche must be legal");
        let snapshot = budget
            .try_reserve(
                OutboundClass::Snapshot,
                codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES,
            )
            .expect("the exact snapshot tranche must coexist with topology");
        let exact = budget.snapshot();
        assert_eq!(exact.retained_bytes, OUTBOUND_ACCOUNTED_MAX_RETAINED_BYTES);
        assert_eq!(
            exact.topology_retained_bytes,
            TOPOLOGY_FENCE_MAX_RETAINED_BYTES
        );
        assert_eq!(
            exact.snapshot_retained_bytes,
            codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES
        );

        drop(snapshot);
        assert_eq!(
            budget
                .try_reserve(OutboundClass::Topology, 1)
                .expect_err("topology must not borrow the released snapshot tranche"),
            OutboundBudgetLimit::TopologyRetainedBytes,
        );
        drop(topology);

        let snapshot = budget
            .try_reserve(
                OutboundClass::Snapshot,
                codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES,
            )
            .expect("reacquire exact snapshot tranche");
        assert_eq!(
            budget
                .try_reserve(OutboundClass::Snapshot, 1)
                .expect_err("snapshot must not borrow the free topology tranche"),
            OutboundBudgetLimit::SnapshotRetainedBytes,
        );
        drop(snapshot);
        assert_outbound_budget_live_counters_zero(&budget);
    }

    fn take_written_pdu(item_rx: &Receiver<Item>) -> DecodedPdu {
        match item_rx.try_recv().expect("queued dispatch item") {
            Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu { decoded, .. })) => *decoded,
            Item::WritePdu(WritePayload::Encoded(frame)) => {
                let mut cursor = Cursor::new(frame.bytes.as_slice());
                let decoded = Pdu::decode(&mut cursor).expect("decode queued outbound frame");
                assert_eq!(
                    cursor.position() as usize,
                    frame.bytes.len(),
                    "one queued encoded item must contain exactly one PDU frame"
                );
                decoded
            }
            other => panic!("expected queued PDU, got {other:?}"),
        }
    }

    fn drain_queued_write_pdus(item_rx: &Receiver<Item>, terminal: &DispatchTerminal) -> Vec<u8> {
        let mut deferred_item = None;
        let mut stream = ChunkedDispatchStream {
            max_write_size: Some(257),
            ..ChunkedDispatchStream::default()
        };
        loop {
            let next = if let Some(item) = deferred_item.take() {
                item
            } else {
                match item_rx.try_recv() {
                    Ok(item) => item,
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                }
            };
            let Item::WritePdu(payload) = next else {
                panic!("wire-drain helper only accepts write PDUs, got {next:?}");
            };
            let mut pending = prepare_pending_outbound_batch(
                payload,
                item_rx,
                &mut deferred_item,
                CompressionMode::Auto,
                terminal,
            )
            .expect("queued test PDU should prepare for outbound service");
            loop {
                match promise::spawn::block_on(service_pending_outbound(
                    &mut stream,
                    &mut pending,
                    None,
                    terminal,
                ))
                .expect("queued test PDU should make outbound progress")
                {
                    OutboundService::Progress => {}
                    OutboundService::Complete => break,
                    OutboundService::Readable => {
                        panic!("non-readable test stream published inbound readiness")
                    }
                    OutboundService::Terminal => {
                        panic!("live test connection became terminal during wire drain")
                    }
                }
            }
        }
        stream.bytes
    }

    fn test_topology_pending(
        budget: &Arc<OutboundBudget>,
        terminal: &DispatchTerminal,
    ) -> PendingOutboundBatch {
        let reservation = budget
            .try_reserve(OutboundClass::Topology, 1024)
            .expect("reserve test topology frame");
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        prepare_pending_outbound_batch(
            WritePayload::Typed(ReservedDecodedPdu {
                decoded: queued_pong(1),
                reservation,
                emission_authority: ServerEmissionAuthority::Ordinary,
            }),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            terminal,
        )
        .expect("prepare test topology frame")
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
        assert_eq!(response.output.len(), 0);
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
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let topology = TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x5a; 16]),
        );

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
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("serial-zero request must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert!(terminal_rx.is_empty(), "terminal reason must be sticky");
        assert!(
            item_rx.is_empty(),
            "rejected request must emit no transcript"
        );
        assert!(matches!(
            &topology.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&topology);
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
    fn dispatch_rejects_wrong_wire_direction_before_handler_mutation() {
        let mux = Arc::new(Mux::new(None));
        let (sender, captured) = capturing_pdu_sender();
        let mut handler = SessionHandler::new_for_mux(sender, Arc::clone(&mux));
        let topology = idle_topology_coordinator();

        let error = dispatch_client_request(
            &mut handler,
            &topology,
            DecodedPdu {
                serial: 2,
                pdu: Pdu::Pong(Pong {}),
            },
        )
        .expect_err("server reply on the client request lane must fail closed");
        assert!(format!("{error:#}").contains("without client-request direction authority"));
        assert!(captured.lock().is_empty());
        assert_eq!(mux.iter_clients().len(), 0);
    }

    #[test]
    fn dispatch_binds_sampled_input_to_its_exact_connection_stream() {
        let mux = Arc::new(Mux::new(None));
        let (sender, captured) = capturing_pdu_sender();
        let stream_id = TopologyStreamId::from_bytes([0x94; 16]);
        let mut handler = SessionHandler::new_for_session_with_topology_stream(
            sender,
            SessionOwner::new(mux),
            stream_id,
        );
        let (item_tx, _item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, _terminal_rx) = DispatchTerminal::channel();
        let topology = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);

        dispatch_client_request(
            &mut handler,
            &topology,
            DecodedPdu {
                serial: 3,
                pdu: Pdu::SendKeyDownTracedV1(sampled_key_request()),
            },
        )
        .expect("valid sampled input should reach the session handler");

        let response = captured.lock();
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].serial, 3);
        match &response[0].pdu {
            Pdu::ErrorResponse(error) => {
                assert_eq!(error.code, codec::MuxErrorCode::PANE_NOT_FOUND);
                assert_eq!(
                    error.request_ident,
                    <codec::SendKeyDownTracedV1 as codec::PduWireIdent>::IDENT
                );
                error.validate().expect("missing pane error must be canonical");
            }
            other => panic!("expected missing-pane response after trace admission, got {other:?}"),
        }
    }

    #[test]
    fn terminal_cancels_partial_client_decode_without_repolling_stream() {
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut stream = PendingWriteThenReadableDispatchStream::default();
        let read_polls = Arc::clone(&stream.read_polls);
        let mut decode = Box::pin(decode_client_pdu_or_terminal(&mut stream, &terminal_rx));
        let mut cx = Context::from_waker(std::task::Waker::noop());

        assert!(matches!(decode.as_mut().poll(&mut cx), Poll::Pending));
        let polls_before_terminal = read_polls.load(Ordering::Relaxed);
        assert!(polls_before_terminal > 0);

        terminal.trip(OUTBOUND_BUDGET_OVERFLOW);
        let Poll::Ready(Err(ClientDecodeError::Terminal(reason))) = decode.as_mut().poll(&mut cx)
        else {
            panic!("terminal event must cancel a partial client frame decode");
        };
        assert_eq!(reason, OUTBOUND_BUDGET_OVERFLOW);
        assert_eq!(
            read_polls.load(Ordering::Relaxed),
            polls_before_terminal,
            "a ready terminal event must win without polling the partial decode again"
        );
    }

    #[test]
    fn ordinary_ping_materializes_through_dormant_exact_render_selector() {
        let mut wire = Vec::new();
        Pdu::Ping(Ping {})
            .encode(&mut wire, 41)
            .expect("ordinary Ping frame should encode");
        let wire_len = wire.len();
        let mut stream = PartialFrameDisconnectStream::new(wire, usize::MAX);
        let (_terminal, terminal_rx) = DispatchTerminal::channel();

        let decoded =
            promise::spawn::block_on(decode_client_pdu_or_terminal(&mut stream, &terminal_rx))
                .expect("ordinary Ping must retain the materializing decode path");
        assert_eq!(decoded.serial, 41);
        assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
        assert_eq!(
            stream.cursor, wire_len,
            "ordinary materialization must consume exactly one complete frame"
        );
    }

    #[test]
    fn dormant_exact_render_headers_are_rejected_before_body_admission() {
        // len=33, serial=1, and IDs 91/92 each have a one-byte LEB128
        // encoding, leaving an opaque 31-byte body after the three-byte
        // header. The payload is deliberately not a valid schema value: the
        // dormant-family authority must never inspect it.
        for ident in [
            <codec::GetPaneRenderDeliveryV1 as codec::PduWireIdent>::IDENT,
            <codec::GetPaneRenderDeliveryV1Response as codec::PduWireIdent>::IDENT,
        ] {
            let mut wire = vec![
                33,
                1,
                u8::try_from(ident).expect("test PDU ID fits one byte"),
            ];
            wire.extend_from_slice(&[0xa5; 31]);
            let mut stream = PartialFrameDisconnectStream::new(wire, usize::MAX);
            let (_terminal, terminal_rx) = DispatchTerminal::channel();

            let error =
                promise::spawn::block_on(decode_client_pdu_or_terminal(&mut stream, &terminal_rx))
                    .expect_err("dormant exact-render family must fail from its header");
            let ClientDecodeError::Decode(error) = error else {
                panic!("expected header-policy decode error, got {error:?}");
            };
            assert!(
                format!("{error:#}").contains("dormant exact render-delivery family"),
                "unexpected dormant-family error: {error:#}"
            );
            assert_eq!(
                stream.cursor, 3,
                "dormant PDU {ident} body must remain completely unread"
            );
        }
    }

    #[test]
    fn compressed_dormant_exact_render_headers_are_rejected_before_body_admission() {
        // Unsigned LEB128 for `(1 << 63) | 33`: the compression flag plus a
        // 33-byte frame body. Serial=1 and IDs 91/92 then leave 31 opaque
        // compressed bytes. The selector must reject before touching them.
        for ident in [
            <codec::GetPaneRenderDeliveryV1 as codec::PduWireIdent>::IDENT,
            <codec::GetPaneRenderDeliveryV1Response as codec::PduWireIdent>::IDENT,
        ] {
            let mut wire = vec![
                0xa1,
                0x80,
                0x80,
                0x80,
                0x80,
                0x80,
                0x80,
                0x80,
                0x80,
                0x01,
                1,
                u8::try_from(ident).expect("test PDU ID fits one byte"),
            ];
            let header_bytes = wire.len();
            wire.extend_from_slice(&[0xa5; 31]);
            let mut stream = PartialFrameDisconnectStream::new(wire, usize::MAX);
            let (_terminal, terminal_rx) = DispatchTerminal::channel();

            let error =
                promise::spawn::block_on(decode_client_pdu_or_terminal(&mut stream, &terminal_rx))
                    .expect_err("compressed dormant exact-render family must fail from its header");
            let ClientDecodeError::Decode(error) = error else {
                panic!("expected compressed header-policy decode error, got {error:?}");
            };
            assert!(
                format!("{error:#}").contains("dormant exact render-delivery family"),
                "unexpected compressed dormant-family error: {error:#}"
            );
            assert_eq!(
                stream.cursor, header_bytes,
                "compressed dormant PDU {ident} body must remain completely unread"
            );
        }
    }

    #[test]
    fn request_dispatch_admission_rejects_terminal_and_releases_before_response_reentry() {
        let rejected_mux = Arc::new(Mux::new(None));
        let (rejected_sender, rejected_responses) = capturing_pdu_sender();
        let mut rejected_handler =
            SessionHandler::new_for_mux(rejected_sender, Arc::clone(&rejected_mux));
        let rejected_topology = idle_topology_coordinator();
        rejected_topology.terminal.trip(OUTBOUND_BUDGET_OVERFLOW);
        let rejected = dispatch_client_request_if_admitted(
            &mut rejected_handler,
            &rejected_topology,
            &rejected_topology.terminal,
            DecodedPdu {
                serial: 90,
                pdu: Pdu::SetClientId(codec::SetClientId {
                    client_id: mux::client::ClientId::new(),
                    is_proxy: false,
                }),
            },
            None,
            None,
        )
        .expect("terminal dispatch admission should be an explicit outcome");
        assert_eq!(rejected, RequestDispatchOutcome::Terminal);
        assert_eq!(rejected_mux.iter_clients().len(), 0);
        assert!(rejected_responses.lock().is_empty());

        let (coordinator, item_rx, _terminal_rx, _, _) = bound_topology_coordinator();
        let coordinator = Arc::new(coordinator);
        let sender = PduSender::new({
            let coordinator = Arc::clone(&coordinator);
            move |pdu, delivery_class| coordinator.queue_response(pdu, delivery_class)
        });
        let mux = Arc::new(Mux::new(None));
        let mut handler = SessionHandler::new_for_mux(sender, mux);

        let dispatched = dispatch_client_request_if_admitted(
            &mut handler,
            &coordinator,
            &coordinator.terminal,
            DecodedPdu {
                serial: 91,
                pdu: Pdu::Ping(Ping {}),
            },
            None,
            None,
        )
        .expect("admitted Ping should synchronously enqueue its Pong response");
        assert_eq!(dispatched, RequestDispatchOutcome::Dispatched);
        assert!(
            coordinator.terminal.admission.try_lock().is_some(),
            "request admission must be released before response callbacks re-enter it"
        );
        let response = take_written_pdu(&item_rx);
        assert_eq!(response.serial, 91);
        assert_eq!(response.pdu, Pdu::Pong(Pong {}));
    }

    #[test]
    fn topology_fence_queues_snapshot_before_reordered_contiguous_events() {
        let (coordinator, item_rx, _terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(41, &fenced_snapshot_request())
            .expect("begin coherent topology fence");

        assert!(
            coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneRemoved(2)),)
        );
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );
        assert!(
            item_rx.is_empty(),
            "events remain quarantined until the coherent snapshot response"
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 41,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("complete coherent topology fence");

        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 41);
        assert!(matches!(snapshot.pdu, Pdu::ListPanesCoherentResponse(_)));
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
    fn coherent_snapshot_metadata_owns_exact_snapshot_retention_and_bounds_queued_refreshes() {
        const TITLE_BYTES: usize = 64 * 1024;
        const TITLE_COUNT: usize = 48;
        let expected_retained = TITLE_BYTES * TITLE_COUNT;
        let snapshot_limit = OutboundRetainedClass::Snapshot.maximum();
        let admitted_snapshots = snapshot_limit / expected_retained;
        assert!(expected_retained < snapshot_limit);
        assert!(admitted_snapshots >= 2);
        assert!(expected_retained * admitted_snapshots <= snapshot_limit);
        assert!(expected_retained * (admitted_snapshots + 1) > snapshot_limit);

        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        coordinator
            .begin_fence(4_101, &fenced_snapshot_request())
            .expect("begin first metadata-retained fence");
        let first = coherent_snapshot_response_with_tab_titles(
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
            (0..TITLE_COUNT).map(|_| "x".repeat(TITLE_BYTES)).collect(),
        );
        assert_eq!(
            coherent_snapshot_metadata_retained_bytes(&first)
                .expect("exact metadata recount must fit"),
            expected_retained
        );
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 4_101,
                    pdu: Pdu::ListPanesCoherentResponse(first),
                },
                PduDeliveryClass::Control,
            )
            .expect("first bounded snapshot must enter the writer queue");
        let retained = coordinator.outbound_budget.snapshot();
        assert_eq!(retained.snapshot_retained_bytes, expected_retained);
        assert_eq!(retained.topology_retained_bytes, 0);
        assert_eq!(retained.retained_bytes, expected_retained);
        assert_eq!(retained.total_slots, 1);
        assert_eq!(retained.bulk_slots, 0);

        for snapshot_index in 1..admitted_snapshots {
            let serial = 4_101 + u64::try_from(snapshot_index).unwrap();
            coordinator
                .begin_fence(serial, &fenced_snapshot_request())
                .expect("begin bounded refresh while prior snapshots remain queued");
            let fill = char::from(b'a' + u8::try_from(snapshot_index % 26).unwrap());
            let response = coherent_snapshot_response_with_tab_titles(
                stream_id,
                session_incarnation,
                TopologyRevision::INITIAL,
                (0..TITLE_COUNT)
                    .map(|_| fill.to_string().repeat(TITLE_BYTES))
                    .collect(),
            );
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial,
                        pdu: Pdu::ListPanesCoherentResponse(response),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("snapshot frame tranche must admit its exact remaining capacity");
        }

        let overflow_serial = 4_101 + u64::try_from(admitted_snapshots).unwrap();
        coordinator
            .begin_fence(overflow_serial, &fenced_snapshot_request())
            .expect("begin the first refresh beyond the snapshot-frame tranche");
        let overflow = coherent_snapshot_response_with_tab_titles(
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
            (0..TITLE_COUNT).map(|_| "z".repeat(TITLE_BYTES)).collect(),
        );
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: overflow_serial,
                    pdu: Pdu::ListPanesCoherentResponse(overflow),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("queued metadata snapshots must share one finite snapshot-frame tranche");
        assert!(format!("{error:#}").contains("snapshot_retained_bytes"));
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("metadata retention overflow must terminate the stream"),
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert_eq!(
            item_rx.len(),
            admitted_snapshots,
            "rejected refresh must never publish"
        );
        for _ in 0..admitted_snapshots {
            let _snapshot = take_written_pdu(&item_rx);
        }
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn future_enabled_ordered_fence_emits_pdu87_before_exact_pdu90_state() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                86,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for frozen ordered state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");

        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0x91; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0x92; 32]),
                    window: frozen,
                },
            ),
        ));
        assert!(
            item_rx.is_empty(),
            "PDU90 must remain behind its establishing PDU87 fence"
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 86,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("complete future-enabled ordered topology fence");

        let queued = coordinator.outbound_budget.snapshot();
        assert_eq!(queued.total_slots, 2);
        assert_eq!(queued.bulk_slots, 1);
        assert!(
            queued.retained_bytes > 0,
            "encoded PDU87 and retained PDU90 must both own accounted bytes"
        );
        assert!(queued.snapshot_retained_bytes > 0);
        assert!(queued.topology_retained_bytes > 0);
        assert_eq!(
            queued.retained_bytes,
            queued.snapshot_retained_bytes + queued.topology_retained_bytes,
        );

        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 86);
        let Pdu::ListPanesOrderedV1Response(snapshot) = snapshot.pdu else {
            panic!("expected the authorized PDU87 fence response");
        };
        snapshot
            .validate_for_request(&request)
            .expect("authorized PDU87 must remain request-correlated");
        let after_snapshot = coordinator.outbound_budget.snapshot();
        assert_eq!(after_snapshot.total_slots, 1);
        assert_eq!(after_snapshot.bulk_slots, 1);
        assert_eq!(after_snapshot.snapshot_retained_bytes, 0);
        assert!(after_snapshot.topology_retained_bytes > 0);
        assert_eq!(
            after_snapshot.retained_bytes,
            after_snapshot.topology_retained_bytes,
        );
        assert!(after_snapshot.retained_bytes < queued.retained_bytes);
        let event = take_written_pdu(&item_rx);
        assert_eq!(event.serial, 0);
        let Pdu::WindowOrderEventV1(event) = event.pdu else {
            panic!("expected ordered-window PDU90 after the PDU87 cut");
        };
        event
            .validate()
            .expect("preconverted PDU90 must satisfy the complete wire contract");
        assert_eq!(event.stream_id, stream_id);
        assert_eq!(event.session_incarnation, session_incarnation);
        assert_eq!(event.topology_revision, TopologyRevision::new(1));
        assert_eq!(event.windows.len(), 1);
        assert_eq!(
            event.windows[0].window_id.get(),
            u64::try_from(window_id).expect("test window id fits u64")
        );
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_fence_transport_flushes_pdu87_before_first_pdu90_byte() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                87,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for frozen ordered state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");

        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0xa1; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0xa2; 32]),
                    window: frozen,
                },
            ),
        ));
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 87,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("publish the ordered snapshot and its contiguous successor");

        let queued = coordinator.outbound_budget.snapshot();
        assert_eq!(queued.total_slots, 2);
        assert_eq!(queued.bulk_slots, 1);
        assert!(queued.retained_bytes > 0);
        assert!(queued.snapshot_retained_bytes > 0);
        assert!(queued.topology_retained_bytes > 0);

        let first = item_rx.try_recv().expect("queued PDU87 control response");
        let Item::WritePdu(first) = first else {
            panic!("ordered fence must publish PDU87 as a write item");
        };
        let mut deferred_item = None;
        let mut first_pending = prepare_pending_outbound_batch(
            first,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &coordinator.terminal,
        )
        .expect("prepare PDU87 while preserving its PDU90 class boundary");
        assert!(
            matches!(
                deferred_item.as_ref(),
                Some(Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu {
                    decoded,
                    ..
                }))) if matches!(&decoded.pdu, Pdu::WindowOrderEventV1(_))
            ),
            "the first PDU90 must remain typed and deferred until PDU87 flushes"
        );
        assert!(item_rx.is_empty());
        let prepared = coordinator.outbound_budget.snapshot();
        assert_eq!(prepared.total_slots, 2);
        assert_eq!(prepared.bulk_slots, 1);
        assert!(prepared.retained_bytes > 0);

        let mut stream = ChunkedDispatchStream {
            max_write_size: Some(17),
            ..ChunkedDispatchStream::default()
        };
        loop {
            match promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut first_pending,
                None,
                &coordinator.terminal,
            ))
            .expect("PDU87 transport service must succeed")
            {
                OutboundService::Progress => {}
                OutboundService::Complete => break,
                other => panic!("unexpected PDU87 transport outcome: {other:?}"),
            }
        }
        let pdu87_end = stream.bytes.len();
        assert_eq!(
            stream.flush_offsets,
            [pdu87_end],
            "PDU87 must be completely written and flushed before any PDU90 byte"
        );
        let mut first_cursor = Cursor::new(stream.bytes.as_slice());
        let first_wire = Pdu::decode(&mut first_cursor).expect("decode flushed PDU87 frame");
        assert_eq!(first_wire.serial, 87);
        assert!(matches!(first_wire.pdu, Pdu::ListPanesOrderedV1Response(_)));
        assert_eq!(first_cursor.position() as usize, pdu87_end);
        drop(first_pending);
        let after_pdu87 = coordinator.outbound_budget.snapshot();
        assert_eq!(after_pdu87.total_slots, 1);
        assert_eq!(after_pdu87.bulk_slots, 1);
        assert_eq!(after_pdu87.snapshot_retained_bytes, 0);
        assert!(after_pdu87.topology_retained_bytes > 0);
        assert_eq!(
            after_pdu87.retained_bytes,
            after_pdu87.topology_retained_bytes,
        );
        assert!(after_pdu87.retained_bytes < prepared.retained_bytes);

        let Item::WritePdu(second) = deferred_item
            .take()
            .expect("PDU90 must remain deferred until the PDU87 flush completes")
        else {
            panic!("deferred ordered event must remain a write item");
        };
        let mut second_pending = prepare_pending_outbound_batch(
            second,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &coordinator.terminal,
        )
        .expect("prepare the deferred PDU90 after the PDU87 flush");
        loop {
            match promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut second_pending,
                None,
                &coordinator.terminal,
            ))
            .expect("PDU90 transport service must succeed")
            {
                OutboundService::Progress => {}
                OutboundService::Complete => break,
                other => panic!("unexpected PDU90 transport outcome: {other:?}"),
            }
        }
        assert_eq!(
            stream.flush_offsets,
            [pdu87_end, stream.bytes.len()],
            "PDU90 must occupy a distinct transport flush epoch after PDU87"
        );
        let mut second_cursor = Cursor::new(&stream.bytes[pdu87_end..]);
        let second_wire = Pdu::decode(&mut second_cursor).expect("decode flushed PDU90 frame");
        assert_eq!(second_wire.serial, 0);
        assert!(matches!(second_wire.pdu, Pdu::WindowOrderEventV1(_)));
        assert_eq!(
            second_cursor.position() as usize,
            stream.bytes.len() - pdu87_end
        );
        drop(second_pending);
        assert!(deferred_item.is_none());
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_snapshot_known_full_queue_fails_before_response_validation() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 0,
                    pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 31 }),
                },
                PduDeliveryClass::Bulk,
            )
            .expect("queue one legitimate predecessor through the connection coordinator");
        let predecessor_budget = coordinator.outbound_budget.snapshot();
        assert_eq!(predecessor_budget.total_slots, 1);
        assert_eq!(predecessor_budget.bulk_slots, 1);
        assert_eq!(predecessor_budget.retained_bytes, 0);

        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                188,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        let mut invalid = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        );
        invalid.domain_binding_id = codec::DomainBindingId::from_bytes([0xee; 16]);

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 188,
                    pdu: Pdu::ListPanesOrderedV1Response(invalid),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("a known-full physical queue must reject PDU87 before q-sized work");
        assert!(
            format!("{error:#}").contains("item queue is full"),
            "queue preflight must precede the deliberately invalid response: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("known-full PDU87 preflight must terminate the connection"),
            RESPONSE_QUEUE_FAILURE,
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_eq!(coordinator.outbound_budget.snapshot(), predecessor_budget);

        let predecessor = take_written_pdu(&item_rx);
        assert_eq!(predecessor.serial, 0);
        assert_eq!(
            predecessor.pdu,
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 31 })
        );
        assert!(item_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn ordered_snapshot_dispatch_validates_arena_and_windows_exactly_once() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                0x871,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        let response = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        );

        codec::debug_reset_ordered_snapshot_validation_passes();
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 0x871,
                    pdu: Pdu::ListPanesOrderedV1Response(response),
                },
                PduDeliveryClass::Control,
            )
            .expect("valid ordered snapshot must enqueue");
        assert_eq!(
            codec::debug_ordered_snapshot_validation_passes(),
            codec::OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 1,
            },
            "dispatch request binding must be the sole q-sized structural scan",
        );

        let queued = take_written_pdu(&item_rx);
        assert_eq!(queued.serial, 0x871);
        assert!(matches!(queued.pdu, Pdu::ListPanesOrderedV1Response(_)));
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn ordered_snapshot_dispatch_rejects_malformed_arena_before_proof_or_encoding() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                0x872,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        let mut response = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        );
        let codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) = &mut response.outcome else {
            panic!("ordered snapshot fixture must contain a snapshot");
        };
        let (trees, mut nodes, window_titles) = snapshot.panes.clone().into_parts();
        nodes.push(mux::tab::PaneArenaNode::Empty);
        snapshot.panes = mux::tab::PaneArena::from_unvalidated_parts(trees, nodes, window_titles);

        codec::debug_reset_ordered_snapshot_validation_passes();
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 0x872,
                    pdu: Pdu::ListPanesOrderedV1Response(response),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("malformed arena must never acquire dispatch encoding authority");
        assert!(
            format!("{error:#}").contains("pane arena references 0 nodes but carries 1"),
            "unexpected malformed-arena rejection: {error:#}",
        );
        assert_eq!(
            codec::debug_ordered_snapshot_validation_passes(),
            codec::OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 0,
            },
            "failed validation must not fall through to field serialization",
        );
        assert!(item_rx.is_empty());
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("malformed ordered snapshot must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert_outbound_budget_live_counters_zero(&coordinator.outbound_budget);
    }

    #[test]
    fn ordered_snapshot_exact_reservation_rejects_class_overflow_and_releases_outside_fence() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                189,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        let prior_snapshot = coordinator
            .outbound_budget
            .try_reserve(
                OutboundClass::Snapshot,
                codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES - 1,
            )
            .expect("reserve all but one byte of the snapshot-specific tranche");

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 189,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("the exact encoded PDU87 allocation must not borrow topology headroom");
        assert!(
            format!("{error:#}").contains("snapshot_retained_bytes"),
            "unexpected ordered-snapshot budget rejection: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("PDU87 snapshot-class overflow must terminate the connection"),
            OUTBOUND_BUDGET_OVERFLOW,
        );
        assert!(item_rx.is_empty());
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        let retained = coordinator.outbound_budget.snapshot();
        assert_eq!(retained.total_slots, 1);
        assert_eq!(retained.bulk_slots, 0);
        assert_eq!(retained.topology_retained_bytes, 0);
        assert_eq!(
            retained.snapshot_retained_bytes,
            codec::MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES - 1,
        );
        drop(prior_snapshot);
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_snapshot_post_preflight_queue_full_is_sticky_terminal() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                195,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let competing_tx = coordinator.item_tx.clone();
        coordinator.set_before_ordered_snapshot_publish_hook(move || {
            competing_tx
                .try_send(Item::Readable)
                .expect("test barrier must fill the queue after PDU87 preflight");
        });
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 195,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("PDU87 must recheck the physical FIFO after its optimistic preflight");
        assert!(
            format!("{error:#}").contains("item queue is full"),
            "unexpected post-preflight PDU87 rejection: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("post-preflight PDU87 loss must terminate the connection"),
            RESPONSE_QUEUE_FAILURE,
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert!(matches!(item_rx.try_recv(), Ok(Item::Readable)));
        assert!(item_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_snapshot_post_preflight_queue_closed_is_sticky_terminal() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                196,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        coordinator.set_before_ordered_snapshot_publish_hook(move || drop(item_rx));

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 196,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("PDU87 must fail closed when the FIFO closes after preflight");
        assert!(
            format!("{error:#}").contains("item queue is closed"),
            "unexpected post-preflight PDU87 close: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("post-preflight PDU87 close must terminate the connection"),
            RESPONSE_QUEUE_FAILURE,
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_snapshot_queue_close_after_final_validation_is_sticky_terminal() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                1_961,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let reached_final_validation = Arc::new(AtomicBool::new(false));
        let reached_final_validation_for_hook = Arc::clone(&reached_final_validation);
        coordinator.set_after_ordered_snapshot_validation_hook(move || {
            reached_final_validation_for_hook.store(true, Ordering::SeqCst);
            drop(item_rx);
        });

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 1_961,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("PDU87 must fail closed when its FIFO closes after final validation");

        assert!(
            reached_final_validation.load(Ordering::SeqCst),
            "the queue failure must be injected only after final fence validation"
        );
        assert!(
            format!("{error:#}").contains("item queue is closed"),
            "unexpected post-validation PDU87 close: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("post-validation PDU87 close must terminate the connection"),
            RESPONSE_QUEUE_FAILURE,
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn first_ordered_event_queue_loss_preserves_success_for_queued_pdu87() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator_with_item_capacity(1);
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                190,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for ordered queue-loss state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered queue-loss window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0xb1; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0xb2; 32]),
                    window: frozen,
                },
            ),
        ));

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 190,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("published PDU87 must remain admitted when the following PDU90 cannot queue");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("first-PDU90 queue loss must terminate the connection"),
            NOTIFICATION_QUEUE_OVERFLOW,
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        let only_snapshot = coordinator.outbound_budget.snapshot();
        assert_eq!(only_snapshot.total_slots, 1);
        assert_eq!(only_snapshot.bulk_slots, 0);
        assert_eq!(only_snapshot.topology_retained_bytes, 0);
        assert!(only_snapshot.snapshot_retained_bytes > 0);

        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 190);
        assert!(matches!(snapshot.pdu, Pdu::ListPanesOrderedV1Response(_)));
        assert!(
            item_rx.is_empty(),
            "failed PDU90 must never reach the queue"
        );
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn first_ordered_event_queue_close_preserves_success_for_published_pdu87() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                197,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for ordered queue-close state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered queue-close window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0xb3; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0xb4; 32]),
                    window: frozen,
                },
            ),
        ));

        let (close_tx, close_rx) = std::sync::mpsc::sync_channel(0);
        let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(0);
        let closer = std::thread::spawn(move || {
            close_rx
                .recv()
                .expect("PDU87 publication barrier must release the closer");
            drop(item_rx);
            closed_tx
                .send(())
                .expect("queue closer must acknowledge receiver destruction");
        });
        coordinator.set_after_ordered_snapshot_publish_hook(move || {
            close_tx
                .send(())
                .expect("PDU87 publication must release the queue closer");
            closed_rx
                .recv()
                .expect("PDU90 admission must wait for deterministic queue closure");
        });

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 197,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("published PDU87 must remain admitted when its FIFO closes before PDU90");
        closer.join().expect("queue closer thread must finish");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("first-PDU90 queue close must terminate the connection"),
            NOTIFICATION_QUEUE_CLOSED,
        );
        assert!(
            terminal_rx.is_empty(),
            "terminal reason must publish exactly once"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        // The already-published PDU87 is owned by the closed channel until the
        // last sender is destroyed; async-channel deliberately retains queued
        // elements after its final receiver disappears. Connection teardown
        // drops the coordinator and must then release that exact reservation.
        let outbound_budget = Arc::clone(&coordinator.outbound_budget);
        drop(coordinator);
        assert_outbound_budget_live_counters_zero(&outbound_budget);
    }

    #[test]
    fn first_ordered_event_encode_overlap_cannot_borrow_snapshot_tranche() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                191,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for ordered encode-overlap state");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered encode-overlap window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");
        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0xc1; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0xc2; 32]),
                    window: frozen,
                },
            ),
        ));
        let retained_event_bytes = coordinator
            .outbound_budget
            .snapshot()
            .topology_retained_bytes;
        assert!(retained_event_bytes > 1);
        let unrelated_topology = coordinator
            .outbound_budget
            .try_reserve(
                OutboundClass::Topology,
                TOPOLOGY_FENCE_MAX_RETAINED_BYTES
                    .checked_sub(retained_event_bytes)
                    .and_then(|bytes| bytes.checked_sub(1))
                    .expect("test event must leave topology accounting headroom"),
            )
            .expect("reserve all but one byte beyond the retained PDU90 state");

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 191,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("PDU87 and the typed PDU90 must fit their separate tranches");
        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 191);

        let payload = match item_rx.try_recv().expect("queued typed PDU90") {
            Item::WritePdu(payload) => payload,
            other => panic!("expected queued PDU90 write, got {other:?}"),
        };
        let mut deferred_item = None;
        let error = prepare_pending_outbound_batch(
            payload,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &coordinator.terminal,
        )
        .expect_err("typed-to-encoded PDU90 overlap must respect its 4 MiB class ceiling");
        assert!(
            format!("{error:#}").contains("topology_retained_bytes"),
            "unexpected PDU90 encode-overlap rejection: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("PDU90 encode-overlap failure must terminate the connection"),
            OUTBOUND_BUDGET_OVERFLOW,
        );
        assert!(deferred_item.is_none());
        let only_unrelated = coordinator.outbound_budget.snapshot();
        assert_eq!(only_unrelated.total_slots, 1);
        assert_eq!(only_unrelated.bulk_slots, 1);
        assert_eq!(only_unrelated.snapshot_retained_bytes, 0);
        assert_eq!(
            only_unrelated.topology_retained_bytes,
            TOPOLOGY_FENCE_MAX_RETAINED_BYTES - retained_event_bytes - 1,
        );
        drop(unrelated_topology);
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_fence_subsumes_snapshot_revision_and_replays_gaps_contiguously() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                192,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for reordered revisions");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to reordered-revision window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");

        for revision in [3_u64, 1, 2] {
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    revision,
                    MuxNotification::WindowOrderChanged {
                        mutation_id: mux::WindowOrderMutationId::new([0xd1; 16], revision,),
                        request_digest: mux::WindowReorderDigest::from_bytes(
                            [u8::try_from(revision).expect("small revision fits u8"); 32]
                        ),
                        window: frozen.clone(),
                    },
                ),
            ));
        }
        assert!(item_rx.is_empty());

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 192,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("ordered fence must subsume r1 and drain buffered r2/r3");

        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 192);
        assert!(matches!(snapshot.pdu, Pdu::ListPanesOrderedV1Response(_)));
        let mut revisions = Vec::new();
        while let Ok(item) = item_rx.try_recv() {
            let decoded = match item {
                Item::WritePdu(WritePayload::Typed(ReservedDecodedPdu { decoded, .. })) => *decoded,
                other => panic!("expected typed buffered PDU90, got {other:?}"),
            };
            let Pdu::WindowOrderEventV1(event) = decoded.pdu else {
                panic!("ordered gap replay must emit only PDU90 successors");
            };
            revisions.push(event.topology_revision.get());
        }
        assert_eq!(
            revisions,
            [2, 3],
            "the snapshot revision is subsumed and gaps publish only in contiguous order",
        );
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_reorder_authority_exists_only_after_successful_pdu87() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        let reorder = ordered_reorder_request(&request, stream_id, session_incarnation);

        let before = coordinator
            .admit_ordered_reorder(&reorder)
            .expect_err("PDU88 must be rejected before its ordered fence begins");
        assert!(format!("{before:#}").contains("has not been established"));

        coordinator
            .begin_ordered_fence_with_server_capabilities(
                193,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");
        let during = coordinator
            .admit_ordered_reorder(&reorder)
            .expect_err("PDU88 must remain rejected while PDU87 is in flight");
        assert!(format!("{during:#}").contains("has not been established"));

        let response = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        );
        let expected_frame_capacity = Pdu::ListPanesOrderedV1Response(response.clone())
            .encode_frame_with_mode(193, CompressionMode::Auto)
            .expect("measure the exact authorized PDU87 frame allocation")
            .capacity();
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 193,
                    pdu: Pdu::ListPanesOrderedV1Response(response),
                },
                PduDeliveryClass::Control,
            )
            .expect("successful PDU87 must establish the immutable PDU88 token");
        let queued = coordinator.outbound_budget.snapshot();
        assert_eq!(queued.total_slots, 1);
        assert_eq!(queued.bulk_slots, 0);
        assert_eq!(queued.topology_retained_bytes, 0);
        assert_eq!(
            queued.snapshot_retained_bytes, expected_frame_capacity,
            "PDU87 must own exactly its sole encoded Vec allocation",
        );
        let snapshot = take_written_pdu(&item_rx);
        assert_eq!(snapshot.serial, 193);
        let after = coordinator
            .admit_ordered_reorder(&reorder)
            .expect("PDU88 must be admitted after its successful exact fence");
        assert_eq!(after.stream_id(), stream_id);
        assert_eq!(after.session_incarnation(), session_incarnation);
        assert_eq!(after.domain_binding_id(), request.domain_binding_id);
        assert!(
            after
                .negotiated()
                .contains(TopologyCapabilities::WINDOW_REORDER_CAS_V1)
        );
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_snapshot_encoded_route_rejects_zero_serial_and_raw_bytes() {
        let request = ordered_snapshot_request(true);
        let stream_id = TopologyStreamId::from_bytes([0x5a; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa5; 16]);
        let (zero_terminal, zero_terminal_rx) = DispatchTerminal::channel();
        let zero_response = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        )
        .validate_for_request_owned(&request)
        .expect("zero-serial fixture must otherwise be request-correlated");
        let zero_error = AuthorizedOrderedSnapshotFrame::encode(zero_response, 0, &zero_terminal)
            .expect_err("PDU87 serial zero must not acquire correlated-response authority");
        assert!(format!("{zero_error:#}").contains("cannot emit as Unilateral"));
        assert_eq!(
            zero_terminal_rx
                .try_recv()
                .expect("serial-zero PDU87 must trip the connection"),
            OUTBOUND_WIRE_AUTHORITY_FAILURE,
        );

        let response = Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::INITIAL,
        ));
        let forged_authority =
            EncodedPduAuthority::capture(&response, 203, ServerEmissionAuthority::Ordinary);
        let budget = Arc::new(OutboundBudget::default());
        let bytes = vec![0x87];
        let reservation = budget
            .try_reserve(OutboundClass::Snapshot, bytes.capacity())
            .expect("reserve one raw test byte");
        let (raw_terminal, raw_terminal_rx) = DispatchTerminal::channel();
        encode_write_payload(
            WritePayload::Encoded(EncodedOutboundFrame {
                bytes,
                reservation,
                authority: forged_authority,
            }),
            CompressionMode::Never,
            &raw_terminal,
        )
        .expect_err("raw encoded bytes must retain and fail the dormant PDU87 guard");
        assert_eq!(
            raw_terminal_rx
                .try_recv()
                .expect("raw PDU87 authority bypass must trip the connection"),
            DORMANT_OUTBOUND_PROTOCOL_FAILURE,
        );
        assert_outbound_budget_live_counters_zero(&budget);
    }

    #[test]
    fn ordered_fence_rejects_cross_wired_pane_order_projection_before_authority() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                185,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let window_id = 11usize;
        let pane_tab_id = 22usize;
        let ordered_tab_id = 23u64;
        let mut response = ordered_snapshot_response(
            &request,
            stream_id,
            session_incarnation,
            TopologyRevision::new(1),
        );
        let codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) = &mut response.outcome else {
            panic!("ordered snapshot fixture must contain a snapshot");
        };
        snapshot.panes = codec::ordered_pane_arena_from_list_panes(codec::ListPanesResponse {
            tabs: vec![mux::tab::PaneNode::Leaf(mux::tab::PaneEntry {
                window_id,
                tab_id: pane_tab_id,
                pane_id: 33,
                title: "pane".to_string(),
                size: TerminalSize::default(),
                working_dir: None,
                alt_screen_active: false,
                is_active_pane: true,
                is_zoomed_pane: false,
                workspace: "workspace".to_string(),
                cursor_pos: StableCursorPosition::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: None,
            })],
            tab_titles: vec!["pane tab".to_string()],
            window_titles: std::collections::HashMap::from([(window_id, "window".to_string())]),
            floating_panes: Vec::new(),
        })
        .expect("cross-wired ordered-pane fixture must flatten");
        snapshot.ordered_windows = vec![codec::OrderedWindowStateV1 {
            window_id: codec::RemoteWindowId::new(
                u64::try_from(window_id).expect("small window id fits u64"),
            ),
            order_revision: codec::WindowOrderRevision::INITIAL,
            ordered_tab_ids: vec![codec::RemoteTabId::new(ordered_tab_id)],
            active_tab_id: Some(codec::RemoteTabId::new(ordered_tab_id)),
        }];
        response
            .validate_for_request(&request)
            .expect("each PDU87 section must remain individually valid");

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 185,
                    pdu: Pdu::ListPanesOrderedV1Response(response),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("dispatch must reject a cross-wired pane/order projection");
        let message = format!("{error:#}");
        assert!(
            message.contains("validating PDU87 pane/order projection at the dispatch fence"),
            "missing dispatch projection context: {message}"
        );
        assert!(
            message.contains("pane tree 0 identifies window/tab (11, 22), expected (11, 23)"),
            "missing exact cross-projection mismatch: {message}"
        );
        assert!(item_rx.is_empty(), "invalid PDU87 must not enqueue");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("cross-projection mismatch must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect_err("rejected PDU87 must not establish reorder authority");
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
    }

    #[test]
    fn production_dormant_ordered_fence_returns_only_correlated_unsupported_and_legacy_resync() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence(186, &request)
            .expect("begin production-dormant ordered topology fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for dormant ordered notification");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to dormant ordered window");
        drop(window);
        let frozen = mux
            .window_order_snapshot(window_id)
            .expect("test ordered window must be valid")
            .expect("test ordered window must exist");
        let legacy_resync_tab_id = frozen
            .active_tab_id()
            .expect("test window must retain its active tab");

        assert!(coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowOrderChanged {
                    mutation_id: mux::WindowOrderMutationId::new([0x81; 16], 1),
                    request_digest: mux::WindowReorderDigest::from_bytes([0x82; 32]),
                    window: frozen,
                },
            ),
        ));
        assert!(
            item_rx.is_empty(),
            "the dormant fence must quarantine its compact legacy resync"
        );

        let response = ordered_unsupported_response(
            &request,
            stream_id,
            TopologyCapabilities::SERVER_SUPPORTED,
        );
        response
            .validate_for_request(&request)
            .expect("dormant PDU87 must be exactly request-correlated");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 186,
                    pdu: Pdu::ListPanesOrderedV1Response(response),
                },
                PduDeliveryClass::Control,
            )
            .expect("publish correlated unsupported PDU87 and restore legacy delivery");

        let unsupported = take_written_pdu(&item_rx);
        assert_eq!(unsupported.serial, 186);
        let Pdu::ListPanesOrderedV1Response(unsupported) = unsupported.pdu else {
            panic!("production-dormant PDU86 must receive exactly PDU87");
        };
        unsupported
            .validate_for_request(&request)
            .expect("queued unsupported PDU87 must preserve the exact PDU86 binding");
        assert_eq!(
            unsupported.negotiated,
            TopologyCapabilities::FENCED_SNAPSHOT_V1
        );
        let codec::ListPanesOrderedV1Outcome::Unsupported { supported } = unsupported.outcome
        else {
            panic!("production-dormant PDU87 must report Unsupported");
        };
        assert_eq!(supported, TopologyCapabilities::SERVER_SUPPORTED);

        let fallback = take_written_pdu(&item_rx);
        assert_eq!(fallback.serial, 0);
        assert!(matches!(
            fallback.pdu,
            Pdu::TabResized(codec::TabResized { tab_id }) if tab_id == legacy_resync_tab_id
        ));
        assert!(
            item_rx.is_empty(),
            "a dormant ordered fence must never leak PDU90"
        );
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Legacy
        ));

        let reorder = ordered_reorder_request(&request, stream_id, session_incarnation);
        let error = coordinator
            .admit_ordered_reorder(&reorder)
            .expect_err("unsupported PDU87 must not establish reorder authority");
        assert!(
            format!("{error:#}").contains("has not been established"),
            "unexpected missing-authority error: {error:#}"
        );
        assert!(terminal_rx.is_empty());
    }

    #[test]
    fn ordered_contended_restore_uses_exact_max_events_plus_one_retirement_capacity() {
        const MAX_EVENTS: usize = 2;
        const FENCE_SERIAL: u64 = 187;

        // PDU87 consumes the first slot. The first legacy fallback consumes
        // the second, so the final fallback deterministically returns its Item
        // owner after the notification queue becomes full. PDU87 publication
        // remains the successful response linearization point while the later
        // fallback failure retires the stream. Every buffered window-order
        // event also retires one compact ordered-window owner; this makes the
        // outside-lock carrier reach exactly max_events + 1.
        let (item_tx, item_rx) = bounded(MAX_EVENTS);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x6a; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xa6; 16]);
        let coordinator = TopologyStreamCoordinator::new_with_retention_limits(
            item_tx,
            terminal,
            stream_id,
            TopologyRetentionLimits {
                max_events: MAX_EVENTS,
                max_retained_bytes: TOPOLOGY_FENCE_MAX_RETAINED_BYTES,
            },
        );
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::INITIAL)
            .expect("bind exact-carrier topology subscription");
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                FENCE_SERIAL,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled exact-carrier fence");

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register exact-carrier test tab");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach exact-carrier test tab");
        drop(window);
        for revision in 1..=MAX_EVENTS {
            let frozen = mux
                .window_order_snapshot(window_id)
                .expect("exact-carrier test window must be valid")
                .expect("exact-carrier test window must exist");
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    u64::try_from(revision).expect("small revision fits u64"),
                    MuxNotification::WindowOrderChanged {
                        mutation_id: mux::WindowOrderMutationId::new(
                            [0x6b; 16],
                            u64::try_from(revision).expect("small mutation sequence fits u64"),
                        ),
                        request_digest: mux::WindowReorderDigest::from_bytes([0x6c; 32]),
                        window: frozen,
                    },
                ),
            ));
        }
        assert!(item_rx.is_empty());

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: FENCE_SERIAL,
                    pdu: Pdu::ListPanesOrderedV1Response(codec::ListPanesOrderedV1Response {
                        protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                        domain_binding_id: request.domain_binding_id,
                        negotiated: request.supported,
                        stream_id,
                        outcome: codec::ListPanesOrderedV1Outcome::Contended {
                            attempts: 1,
                            first_revision: TopologyRevision::INITIAL,
                            last_revision: TopologyRevision::new(
                                u64::try_from(MAX_EVENTS).expect("small last revision fits u64"),
                            ),
                        },
                    }),
                },
                PduDeliveryClass::Control,
            )
            .expect(
                "a published PDU87 remains successful when later fallback settlement retires the stream",
            );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("the exact carrier boundary must publish one terminal reason"),
            NOTIFICATION_QUEUE_OVERFLOW,
        );
        assert!(terminal_rx.is_empty());
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_eq!(item_rx.len(), MAX_EVENTS);

        let response = take_written_pdu(&item_rx);
        assert!(matches!(
            response,
            DecodedPdu {
                serial: FENCE_SERIAL,
                pdu: Pdu::ListPanesOrderedV1Response(codec::ListPanesOrderedV1Response {
                    outcome: codec::ListPanesOrderedV1Outcome::Contended { .. },
                    ..
                }),
            }
        ));
        let fallback = take_written_pdu(&item_rx);
        assert!(matches!(
            fallback,
            DecodedPdu {
                serial: 0,
                pdu: Pdu::TabResized(_),
            }
        ));
        assert!(item_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_fence_same_serial_wrong_response_family_is_sticky_terminal() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                187,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin future-enabled ordered topology fence");

        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 187,
                    pdu: Pdu::Pong(Pong {}),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("same-serial non-PDU87 must fail its ordered fence");
        assert!(
            format!("{error:#}").contains("non-PDU87 response"),
            "unexpected wrong-family error: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("wrong response family must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 188,
                    pdu: Pdu::Pong(Pong {}),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("no response may enqueue after the terminal family mismatch");
        assert!(item_rx.is_empty());
        assert!(
            terminal_rx.is_empty(),
            "the first terminal reason must remain sticky"
        );
    }

    #[test]
    fn ordered_over_ordered_fence_overlap_is_terminal_and_releases_retained_owner() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                186,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin first ordered topology fence");
        assert!(coordinator.on_notification(
            &Mux::new(None),
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));
        assert!(coordinator.outbound_budget.snapshot().retained_bytes > 0);

        let error = coordinator
            .begin_ordered_fence_with_server_capabilities(
                187,
                &request,
                ordered_window_capabilities(true),
            )
            .expect_err("ordered-over-ordered overlap must fail closed");
        assert!(
            format!("{error:#}").contains("overlapping mux topology snapshot requests"),
            "unexpected ordered-over-ordered error: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("ordered overlap must publish one terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert!(terminal_rx.is_empty());
        assert!(item_rx.is_empty());
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
        coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect_err("PDU88 must remain unavailable after ordered overlap");
        assert!(terminal_rx.is_empty());
    }

    #[test]
    fn coherent_over_ordered_fence_overlap_is_terminal_and_releases_retained_owner() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                187,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin ordered topology fence");
        assert!(coordinator.on_notification(
            &Mux::new(None),
            topology_envelope(1, MuxNotification::PaneAdded(1)),
        ));
        assert!(coordinator.outbound_budget.snapshot().retained_bytes > 0);

        let error = coordinator
            .begin_fence(188, &fenced_snapshot_request())
            .expect_err("coherent-over-ordered overlap must fail closed");
        assert!(
            format!("{error:#}").contains("overlapping coherent mux topology snapshot requests"),
            "unexpected coherent-over-ordered error: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("coherent overlap must publish one terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert!(terminal_rx.is_empty());
        assert!(item_rx.is_empty());
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
        coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect_err("PDU88 must remain unavailable after coherent overlap");
        assert!(terminal_rx.is_empty());
    }

    #[test]
    fn ordered_snapshot_authority_mismatch_matrix_is_terminal_and_owner_clean() {
        #[derive(Clone, Copy)]
        enum Mismatch {
            Serial,
            Stream,
            Binding,
            Negotiated,
            Session,
        }

        for (label, mismatch) in [
            ("serial", Mismatch::Serial),
            ("stream", Mismatch::Stream),
            ("binding", Mismatch::Binding),
            ("negotiated", Mismatch::Negotiated),
            ("session", Mismatch::Session),
        ] {
            let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
                bound_topology_coordinator();
            let request = ordered_snapshot_request(true);
            const REQUEST_SERIAL: u64 = 188;
            coordinator
                .begin_ordered_fence_with_server_capabilities(
                    REQUEST_SERIAL,
                    &request,
                    ordered_window_capabilities(true),
                )
                .expect("begin future-enabled ordered topology fence");
            let mut response = ordered_snapshot_response(
                &request,
                stream_id,
                session_incarnation,
                TopologyRevision::new(1),
            );
            let response_serial = match mismatch {
                Mismatch::Serial => REQUEST_SERIAL + 1,
                Mismatch::Stream => {
                    response.stream_id = TopologyStreamId::from_bytes([0x6e; 16]);
                    REQUEST_SERIAL
                }
                Mismatch::Binding => {
                    response.domain_binding_id = codec::DomainBindingId::from_bytes([0xe1; 16]);
                    REQUEST_SERIAL
                }
                Mismatch::Negotiated => {
                    response.negotiated = ordered_snapshot_foundation();
                    REQUEST_SERIAL
                }
                Mismatch::Session => {
                    let codec::ListPanesOrderedV1Outcome::Snapshot(snapshot) =
                        &mut response.outcome
                    else {
                        unreachable!("ordered response helper must produce a snapshot")
                    };
                    snapshot.session_incarnation = MuxSessionIncarnation::from_bytes([0xe2; 16]);
                    REQUEST_SERIAL
                }
            };

            let _error = coordinator
                .queue_response(
                    DecodedPdu {
                        serial: response_serial,
                        pdu: Pdu::ListPanesOrderedV1Response(response),
                    },
                    PduDeliveryClass::Control,
                )
                .expect_err("mismatched PDU87 authority must fail closed");
            assert_eq!(
                terminal_rx
                    .try_recv()
                    .expect("PDU87 mismatch must publish one terminal reason"),
                TOPOLOGY_PROTOCOL_FAILURE,
                "wrong terminal reason for {label} mismatch",
            );
            assert!(
                terminal_rx.is_empty(),
                "terminal reason must be sticky for {label} mismatch"
            );
            assert!(
                item_rx.is_empty(),
                "{label} mismatch must leave an empty transcript"
            );
            assert!(matches!(
                &coordinator.state.lock().phase,
                TopologyStreamPhase::Exhausted
            ));
            assert_outbound_live_counters_zero(&coordinator);
            coordinator
                .admit_ordered_reorder(&ordered_reorder_request(
                    &request,
                    stream_id,
                    session_incarnation,
                ))
                .expect_err("PDU88 must be unavailable after PDU87 authority failure");
            assert!(
                terminal_rx.is_empty(),
                "PDU88 retry must not replace the {label} mismatch reason"
            );
        }
    }

    #[test]
    fn exact_ordered_refresh_succeeds_but_changed_request_sticky_revokes() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        let server_supported = ordered_window_capabilities(true);

        coordinator
            .begin_ordered_fence_with_server_capabilities(189, &request, server_supported)
            .expect("begin initial future-enabled ordered fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 189,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish initial ordered authority");
        let initial_response = take_written_pdu(&item_rx);
        assert_eq!(initial_response.serial, 189);
        let initial_authority = coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect("initial PDU87 must establish reorder authority");

        coordinator
            .begin_ordered_fence_with_server_capabilities(190, &request, server_supported)
            .expect("an exact PDU86 refresh must be admitted");
        let during_refresh = coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect_err(
                "PDU88 must not borrow authority while an exact PDU87 refresh is in flight",
            );
        assert!(
            format!("{during_refresh:#}").contains("has not been established"),
            "unexpected in-refresh PDU88 rejection: {during_refresh:#}"
        );
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 190,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(2),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("an exact PDU86 refresh must complete");
        let refresh_response = take_written_pdu(&item_rx);
        assert_eq!(refresh_response.serial, 190);
        let refreshed_authority = coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect("exact refresh must preserve reorder authority");
        assert_eq!(refreshed_authority, initial_authority);

        let changed_request = codec::ListPanesOrderedV1 {
            supported: TopologyCapabilities::from_bits(
                request.supported.bits() | TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.bits(),
            ),
            required: ordered_snapshot_foundation(),
            ..request.clone()
        };
        changed_request
            .validate()
            .expect("changed offered and required masks remain a legal PDU86");
        assert_ne!(changed_request, request);
        assert_eq!(
            changed_request.supported.intersection(server_supported),
            request.supported.intersection(server_supported),
            "the changed request deliberately retains the same negotiated intersection"
        );

        let error = coordinator
            .begin_ordered_fence_with_server_capabilities(191, &changed_request, server_supported)
            .expect_err("changed PDU86 authority must revoke instead of refreshing");
        assert!(
            format!("{error:#}").contains("request or negotiated capability change revoked"),
            "unexpected refresh-revocation error: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("changed request must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());

        coordinator
            .begin_ordered_fence_with_server_capabilities(192, &request, server_supported)
            .expect_err("the revocation must remain sticky for an exact retry");
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
    }

    #[test]
    fn malformed_ordered_refresh_at_dispatch_revokes_authority_and_retained_successors() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                193,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin initial future-enabled ordered fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 193,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish ordered authority before malformed refresh");
        assert_eq!(take_written_pdu(&item_rx).serial, 193);

        let mux = Arc::new(Mux::new(None));
        assert!(
            coordinator.on_notification(&mux, topology_envelope(3, MuxNotification::PaneAdded(3)),)
        );
        assert!(item_rx.is_empty());
        assert!(
            coordinator.outbound_budget.snapshot().retained_bytes > 0,
            "a revision gap must retain one successor before revocation"
        );

        let (sender, captured) = capturing_pdu_sender();
        let mut handler = SessionHandler::new_for_mux(sender, mux);
        let malformed = codec::ListPanesOrderedV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION.saturating_add(1),
            ..request.clone()
        };
        let error = dispatch_client_request(
            &mut handler,
            &coordinator,
            DecodedPdu {
                serial: 194,
                pdu: Pdu::ListPanesOrderedV1(malformed),
            },
        )
        .expect_err("malformed PDU86 refresh must revoke dispatch authority");
        assert!(
            format!("{error:#}").contains("validating PDU86"),
            "unexpected malformed-refresh rejection: {error:#}"
        );
        assert!(captured.lock().is_empty());
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("malformed refresh must publish one terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert!(terminal_rx.is_empty(), "terminal reason must remain sticky");
        assert!(item_rx.is_empty(), "revocation must emit no new transcript");
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
        coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect_err("PDU88 must remain unavailable after malformed refresh revocation");
        assert!(
            terminal_rx.is_empty(),
            "PDU88 retry must not replace the reason"
        );
    }

    #[test]
    fn rejected_pdu88_at_dispatch_is_sticky_terminal_and_revokes_inflight_fence() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(
                195,
                &request,
                ordered_window_capabilities(true),
            )
            .expect("begin ordered fence before premature PDU88");
        let mux = Arc::new(Mux::new(None));
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );
        assert!(coordinator.outbound_budget.snapshot().retained_bytes > 0);

        let (sender, captured) = capturing_pdu_sender();
        let mut handler = SessionHandler::new_for_mux(sender, mux);
        let error = dispatch_client_request(
            &mut handler,
            &coordinator,
            DecodedPdu {
                serial: 196,
                pdu: Pdu::ReorderWindowTabsV1(ordered_reorder_request(
                    &request,
                    stream_id,
                    session_incarnation,
                )),
            },
        )
        .expect_err("PDU88 before successful PDU87 must fail at dispatch");
        assert!(
            format!("{error:#}").contains("has not been established"),
            "unexpected premature PDU88 error: {error:#}"
        );
        assert!(captured.lock().is_empty());
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("rejected PDU88 must publish one terminal reason"),
            TOPOLOGY_PROTOCOL_FAILURE,
        );
        assert!(terminal_rx.is_empty());
        assert!(item_rx.is_empty());
        assert!(matches!(
            &coordinator.state.lock().phase,
            TopologyStreamPhase::Exhausted
        ));
        assert_outbound_live_counters_zero(&coordinator);
        let retry = dispatch_client_request_if_admitted(
            &mut handler,
            &coordinator,
            &coordinator.terminal,
            DecodedPdu {
                serial: 197,
                pdu: Pdu::Ping(Ping {}),
            },
            None,
            None,
        )
        .expect("terminal request gate must return an explicit outcome");
        assert_eq!(retry, RequestDispatchOutcome::Terminal);
        assert!(terminal_rx.is_empty(), "terminal reason must remain sticky");
        assert!(captured.lock().is_empty());
    }

    #[test]
    fn exact_ordered_refresh_replays_frozen_successors_in_revision_order() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        let server_supported = ordered_window_capabilities(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(198, &request, server_supported)
            .expect("begin initial future-enabled ordered fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 198,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish the initial ordered stream");
        assert_eq!(take_written_pdu(&item_rx).serial, 198);

        coordinator
            .begin_ordered_fence_with_server_capabilities(199, &request, server_supported)
            .expect("begin an exact ordered refresh");
        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(&mux);
        let tab = Arc::new(mux::tab::Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&tab)
            .expect("register test tab for ordered refresh successors");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach test tab to ordered refresh window");
        drop(window);

        for revision in [3, 2] {
            let frozen = mux
                .window_order_snapshot(window_id)
                .expect("test ordered window must be valid")
                .expect("test ordered window must exist");
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    revision,
                    MuxNotification::WindowOrderChanged {
                        mutation_id: mux::WindowOrderMutationId::new([0xc1; 16], revision,),
                        request_digest: mux::WindowReorderDigest::from_bytes([0xc2; 32]),
                        window: frozen,
                    },
                ),
            ));
        }
        assert!(
            item_rx.is_empty(),
            "refresh successors must remain quarantined until the new PDU87 cut"
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 199,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("refresh must publish PDU87 then every contiguous frozen successor");
        assert_eq!(take_written_pdu(&item_rx).serial, 199);
        for expected_revision in [2, 3] {
            let successor = take_written_pdu(&item_rx);
            assert_eq!(successor.serial, 0);
            let Pdu::WindowOrderEventV1(successor) = successor.pdu else {
                panic!("ordered refresh successor must use PDU90");
            };
            assert_eq!(
                successor.topology_revision,
                TopologyRevision::new(expected_revision),
            );
            assert_eq!(successor.stream_id, stream_id);
            assert_eq!(successor.session_incarnation, session_incarnation);
        }
        assert!(item_rx.is_empty());
        coordinator
            .admit_ordered_reorder(&ordered_reorder_request(
                &request,
                stream_id,
                session_incarnation,
            ))
            .expect("complete refresh must restore PDU88 authority");
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn ordered_refresh_baseline_cannot_regress_published_contiguous_head() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let request = ordered_snapshot_request(true);
        let server_supported = ordered_window_capabilities(true);
        coordinator
            .begin_ordered_fence_with_server_capabilities(193, &request, server_supported)
            .expect("begin initial future-enabled ordered fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 193,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(5),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish revision-five ordered stream");
        let initial = take_written_pdu(&item_rx);
        assert_eq!(initial.serial, 193);

        let mux = Mux::new(None);
        for revision in [6, 7] {
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    revision,
                    MuxNotification::PaneAdded(
                        usize::try_from(revision).expect("small revision fits pane id"),
                    ),
                ),
            ));
            let published = take_written_pdu(&item_rx);
            assert_eq!(published.serial, 0);
            let Pdu::TopologyEvent(published) = published.pdu else {
                panic!("ordered stream ordinary successor must use PDU83");
            };
            assert_eq!(published.revision, TopologyRevision::new(revision));
        }

        coordinator
            .begin_ordered_fence_with_server_capabilities(194, &request, server_supported)
            .expect("begin exact ordered refresh after contiguous publication");
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 194,
                    pdu: Pdu::ListPanesOrderedV1Response(ordered_snapshot_response(
                        &request,
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(6),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("refresh baseline must not precede published revision seven");
        assert!(
            format!("{error:#}").contains("did not match the connection subscription"),
            "unexpected refresh-regression error: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("regressive ordered refresh must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        assert!(item_rx.is_empty());
    }

    #[test]
    fn bound_subscription_discards_delayed_events_at_or_before_its_baseline() {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x5d; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xc6; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::new(5))
            .expect("bind topology subscription at revision five");
        let mux = Mux::new(None);

        for revision in [4, 5] {
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    revision,
                    MuxNotification::PaneAdded(
                        usize::try_from(revision).expect("small revision fits pane id"),
                    ),
                ),
            ));
        }
        assert!(
            item_rx.is_empty(),
            "a delayed predecessor represented by the bound baseline must not reach the wire"
        );
        assert_outbound_live_counters_zero(&coordinator);

        assert!(
            coordinator.on_notification(&mux, topology_envelope(6, MuxNotification::PaneAdded(6)),)
        );
        let Item::Notif(ReservedNotification {
            notification: MuxNotification::PaneAdded(6),
            ..
        }) = item_rx
            .try_recv()
            .expect("the first post-baseline revision must retain legacy delivery")
        else {
            panic!("expected the first post-baseline legacy notification");
        };
        assert!(item_rx.is_empty());
        assert!(terminal_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn topology_fence_event_count_overflow_is_terminal_and_emits_no_transcript() {
        const FENCE_SERIAL: u64 = 300;
        const TINY_EVENT_LIMIT: usize = 2;

        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x6d; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xd6; 16]);
        let coordinator = TopologyStreamCoordinator::new_with_retention_limits(
            item_tx,
            terminal,
            stream_id,
            TopologyRetentionLimits {
                max_events: TINY_EVENT_LIMIT,
                max_retained_bytes: usize::MAX,
            },
        );
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::INITIAL)
            .expect("bind count-limited topology subscription");
        coordinator
            .begin_fence(FENCE_SERIAL, &fenced_snapshot_request())
            .expect("begin count-limited coherent topology fence");
        let mux = Mux::new(None);

        for revision in 1..=TINY_EVENT_LIMIT {
            assert!(coordinator.on_notification(
                &mux,
                topology_envelope(
                    u64::try_from(revision).expect("tiny revision fits u64"),
                    MuxNotification::PaneAdded(revision),
                ),
            ));
        }
        let expected_retained_bytes = RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES
            .checked_mul(TINY_EVENT_LIMIT)
            .expect("tiny retained-event accounting fits usize");
        let retained = coordinator.outbound_budget.snapshot();
        assert_eq!(retained.total_slots, TINY_EVENT_LIMIT);
        assert_eq!(retained.bulk_slots, TINY_EVENT_LIMIT);
        assert_eq!(retained.retained_bytes, expected_retained_bytes);
        assert_eq!(retained.peak_retained_bytes, expected_retained_bytes);
        assert!(
            item_rx.is_empty(),
            "in-fence events must remain quarantined before the count limit trips"
        );

        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(3, MuxNotification::PaneAdded(3)),)
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("the first count overflow must publish one terminal reason"),
            TOPOLOGY_BUFFER_OVERFLOW
        );
        assert!(
            terminal_rx.is_empty(),
            "one semantic loss must publish exactly one terminal reason"
        );
        assert!(
            item_rx.is_empty(),
            "count overflow must emit no partial snapshot or topology transcript"
        );
        {
            let state = coordinator.state.lock();
            assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
            assert!(state.prebind.events.is_empty());
            assert_eq!(state.prebind.retained_bytes, 0);
        }

        assert!(
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial: FENCE_SERIAL,
                        pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                            stream_id,
                            session_incarnation,
                            TopologyRevision::INITIAL,
                        )),
                    },
                    PduDeliveryClass::Control,
                )
                .is_err(),
            "the retired fence request must not publish a response after count overflow"
        );
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(4, MuxNotification::PaneAdded(4)),)
        );
        assert!(
            coordinator
                .begin_fence(FENCE_SERIAL, &fenced_snapshot_request())
                .is_err(),
            "the retired request serial must not reopen the terminal fence"
        );
        assert!(terminal_rx.is_empty());
        assert!(item_rx.is_empty());
        assert_outbound_live_counters_zero(&coordinator);
        assert_eq!(
            coordinator.outbound_budget.snapshot().peak_retained_bytes,
            expected_retained_bytes,
            "count overflow must release live reservations without rewriting the exact high-water mark"
        );
    }

    #[test]
    fn terminal_queue_loss_retires_pending_deferred_fence_before_wire_transcript() {
        const FENCE_SERIAL: u64 = 301;

        // Two slots are enough to create the production pending/deferred
        // boundary and then deterministically reject the fenced response.  A
        // smaller test-only capacity avoids timing, sleeping, or a 4K-item
        // fixture while exercising the same bounded channel and admission
        // gates as the connection loop.
        let (item_tx, item_rx) = bounded(2);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x7d; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0xd7; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        coordinator
            .bind_subscription(session_incarnation, TopologyRevision::INITIAL)
            .expect("bind terminal-transcript topology subscription");
        let mux = Mux::new(None);

        // Model the exact state the production loop can hold while a newly
        // readable request preempts outbound progress: one legacy topology
        // frame is pending and the following control response is deferred at
        // the topology/non-topology accounting boundary.
        assert!(
            coordinator
                .on_notification(&mux, topology_envelope(1, MuxNotification::PaneRemoved(1)),)
        );
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 201,
                    pdu: Pdu::Pong(Pong {}),
                },
                PduDeliveryClass::Control,
            )
            .expect("queue the control successor before the fence request");
        let Item::Notif(ReservedNotification {
            notification: MuxNotification::PaneRemoved(pane_id),
            reservation,
        }) = item_rx
            .try_recv()
            .expect("take the admitted legacy topology predecessor")
        else {
            panic!("the first admitted item must be the legacy topology notification");
        };
        let mut deferred_item = None;
        let mut pending = prepare_unilateral_pdu(
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id }),
            reservation,
            &item_rx,
            &mut deferred_item,
            &coordinator.terminal,
        )
        .expect("prepare the production pending/deferred boundary");
        let Some(Item::WritePdu(WritePayload::Typed(deferred))) = deferred_item.as_ref() else {
            panic!("the control successor must remain one exact deferred typed owner");
        };

        let mut pending_cursor = Cursor::new(pending.bytes.as_slice());
        assert!(matches!(
            Pdu::decode(&mut pending_cursor).expect("decode pending topology predecessor"),
            DecodedPdu {
                serial: 0,
                pdu: Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 1 }),
            }
        ));
        assert_eq!(pending_cursor.position() as usize, pending.bytes.len());
        assert_eq!(
            deferred.decoded.as_ref(),
            &DecodedPdu {
                serial: 201,
                pdu: Pdu::Pong(Pong {}),
            }
        );
        assert_eq!(
            deferred.emission_authority,
            ServerEmissionAuthority::Ordinary
        );
        assert!(item_rx.is_empty());

        coordinator
            .begin_fence(FENCE_SERIAL, &fenced_snapshot_request())
            .expect("admit the coherent topology request while output is pending");
        assert!(
            coordinator.on_notification(&mux, topology_envelope(2, MuxNotification::PaneAdded(2)),)
        );
        assert!(
            item_rx.is_empty(),
            "the in-fence successor must remain quarantined"
        );

        for serial in [302, 303] {
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial,
                        pdu: Pdu::Pong(Pong {}),
                    },
                    PduDeliveryClass::Control,
                )
                .expect("fill one exact tiny-queue slot before semantic loss");
        }
        let queued_before_loss = item_rx.len();
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: FENCE_SERIAL,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("the first rejected fenced response must terminate the connection");
        assert!(
            format!("{error:#}").contains("mux dispatch item queue is full"),
            "the terminal transcript must retain its first semantic-loss cause: {error:#}"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("the first queue loss must publish one terminal reason"),
            RESPONSE_QUEUE_FAILURE
        );
        assert!(terminal_rx.is_empty());
        assert_eq!(
            item_rx.len(),
            queued_before_loss,
            "the rejected snapshot must not displace or append a frame"
        );
        {
            let state = coordinator.state.lock();
            assert!(
                matches!(&state.phase, TopologyStreamPhase::Exhausted),
                "the rejected fence response must retire the in-flight request instead of establishing readiness-equivalent topology authority"
            );
            assert_eq!(state.prebind.retained_bytes, 0);
            assert!(state.prebind.events.is_empty());
        }

        assert!(
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial: 304,
                        pdu: Pdu::Pong(Pong {}),
                    },
                    PduDeliveryClass::Control,
                )
                .is_err(),
            "no later response may be admitted after the first semantic loss"
        );
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(3, MuxNotification::PaneAdded(3)),)
        );
        assert!(
            coordinator
                .begin_fence(FENCE_SERIAL, &fenced_snapshot_request())
                .is_err(),
            "the retired request serial must not reopen a topology fence"
        );
        assert!(terminal_rx.is_empty());
        assert_eq!(item_rx.len(), queued_before_loss);

        // Both already-owned outbound stages consult the same terminal gate.
        // Decode their would-be frames above, then prove that neither stage
        // can add even one byte to the actual connection transcript.
        let mut stream = RecordingDispatchStream::default();
        assert_eq!(
            promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut pending,
                None,
                &coordinator.terminal,
            ))
            .expect("terminal pending service must return an explicit outcome"),
            OutboundService::Terminal
        );
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.bytes.len(), 0);
        assert_eq!(stream.flush_calls.load(Ordering::Relaxed), 0);
        drop(pending);

        let Some(Item::WritePdu(deferred_payload)) = deferred_item.take() else {
            panic!("the exact deferred control frame must still be owned");
        };
        let mut retired_batch = prepare_pending_outbound_batch(
            deferred_payload,
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &coordinator.terminal,
        )
        .expect("already-admitted control frames remain decodable for retirement");
        assert!(deferred_item.is_none());
        assert!(item_rx.is_empty());
        let mut retired_cursor = Cursor::new(retired_batch.bytes.as_slice());
        for expected_serial in [201, 302, 303] {
            assert_eq!(
                Pdu::decode(&mut retired_cursor)
                    .expect("decode one already-admitted retired control frame"),
                DecodedPdu {
                    serial: expected_serial,
                    pdu: Pdu::Pong(Pong {}),
                }
            );
        }
        assert_eq!(
            retired_cursor.position() as usize,
            retired_batch.bytes.len(),
            "the retired decoded transcript must contain exactly the three pre-loss controls"
        );
        assert_eq!(
            promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut retired_batch,
                None,
                &coordinator.terminal,
            ))
            .expect("terminal deferred service must return an explicit outcome"),
            OutboundService::Terminal
        );
        assert_eq!(retired_batch.offset, 0);
        assert_eq!(stream.bytes.len(), 0);
        assert_eq!(stream.flush_calls.load(Ordering::Relaxed), 0);
        drop(retired_batch);

        assert_outbound_live_counters_zero(&coordinator);
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
            .queue_response(
                DecodedPdu {
                    serial: 42,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish topology stream");
        let _snapshot = take_written_pdu(&item_rx);

        assert!(
            coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneRemoved(2)),)
        );
        assert!(
            item_rx.is_empty(),
            "revision two must remain quarantined while revision one is missing"
        );
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

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
        assert!(
            coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneRemoved(2)),)
        );
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 43,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(1),
                    )),
                },
                PduDeliveryClass::Control,
            )
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
            .queue_response(
                DecodedPdu {
                    serial: 44,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(4),
                    )),
                },
                PduDeliveryClass::Control,
            )
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
        assert!(
            coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneRemoved(2)),)
        );
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

        coordinator
            .queue_response(
                DecodedPdu {
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
                },
                PduDeliveryClass::Control,
            )
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
    fn coherent_refresh_cannot_regress_its_established_snapshot_revision() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        coordinator
            .begin_fence(144, &fenced_snapshot_request())
            .expect("begin initial coherent topology fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 144,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(5),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish revision-five coherent stream");
        let _established_snapshot = take_written_pdu(&item_rx);

        coordinator
            .begin_fence(145, &fenced_snapshot_request())
            .expect("begin coherent refresh");
        let error = coordinator
            .queue_response(
                DecodedPdu {
                    serial: 145,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::new(4),
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect_err("a coherent refresh must not regress established authority");

        assert!(
            error
                .to_string()
                .contains("did not match the connection subscription")
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("regressive refresh must trip the connection"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
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
        let mut retained_prefix = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
        let mut overflowing_title = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(item_rx.is_empty());
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneAdded(2)),)
        );
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
        assert_outbound_live_counters_zero(&coordinator);
        assert!(
            coordinator
                .begin_fence(99, &fenced_snapshot_request())
                .is_err(),
            "post-terminal fence admission must fail"
        );
        assert!(
            coordinator
                .queue_response(
                    DecodedPdu {
                        serial: 99,
                        pdu: Pdu::Pong(Pong {}),
                    },
                    PduDeliveryClass::Control,
                )
                .is_err(),
            "post-terminal response admission must fail"
        );
        assert!(item_rx.is_empty());

        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        coordinator
            .begin_fence(46, &fenced_snapshot_request())
            .expect("begin second coherent topology fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 46,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish second topology stream");
        let _snapshot = take_written_pdu(&item_rx);
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );
        let _first = take_written_pdu(&item_rx);
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );
        assert_eq!(
            terminal_rx.try_recv().expect("terminal duplicate reason"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
    }

    #[test]
    fn subscribed_legacy_topology_rejects_one_oversized_owned_title() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        let mux = Mux::new(None);
        let mut title = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES);
        title.push('x');

        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title,
                },
            ),
        ));
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("oversized legacy title reason"),
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(item_rx.is_empty());
        let released = coordinator.outbound_budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn established_topology_rejects_one_oversized_owned_title() {
        let (coordinator, item_rx, terminal_rx, session_incarnation, stream_id) =
            bound_topology_coordinator();
        let mux = Mux::new(None);
        coordinator
            .begin_fence(45, &fenced_snapshot_request())
            .expect("begin oversized established-title fence");
        coordinator
            .queue_response(
                DecodedPdu {
                    serial: 45,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish topology stream");
        let _snapshot = take_written_pdu(&item_rx);
        let mut title = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES);
        title.push('x');

        assert!(!coordinator.on_notification(
            &mux,
            topology_envelope(
                1,
                MuxNotification::WindowTitleChanged {
                    window_id: 1,
                    title,
                },
            ),
        ));
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("oversized established title reason"),
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(item_rx.is_empty());
        let released = coordinator.outbound_budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn topology_capacity_overflow_releases_prebind_and_established_gap_retention() {
        let mux = Mux::new(None);

        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let stream_id = TopologyStreamId::from_bytes([0x6a; 16]);
        let coordinator = TopologyStreamCoordinator::new(item_tx, terminal, stream_id);
        let mut retained_prefix = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
        let mut overflowing_title = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(item_rx.is_empty());
        {
            let state = coordinator.state.lock();
            assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
            assert_eq!(state.prebind.retained_bytes, 0);
            assert!(state.prebind.events.is_empty());
        }
        assert_outbound_live_counters_zero(&coordinator);
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(3, MuxNotification::PaneAdded(3)),)
        );
        assert!(
            terminal_rx.is_empty(),
            "post-overflow prebind ingress must not publish a second terminal reason"
        );
        assert_outbound_live_counters_zero(&coordinator);
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
            .queue_response(
                DecodedPdu {
                    serial: 47,
                    pdu: Pdu::ListPanesCoherentResponse(coherent_snapshot_response(
                        stream_id,
                        session_incarnation,
                        TopologyRevision::INITIAL,
                    )),
                },
                PduDeliveryClass::Control,
            )
            .expect("establish topology stream");
        let _snapshot = take_written_pdu(&item_rx);

        let mut retained_prefix = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
        let mut overflowing_title = String::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES / 2);
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
            OUTBOUND_BUDGET_OVERFLOW
        );
        assert!(item_rx.is_empty());
        {
            let state = coordinator.state.lock();
            assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
            assert_eq!(state.prebind.retained_bytes, 0);
            assert!(state.prebind.events.is_empty());
        }
        assert_outbound_live_counters_zero(&coordinator);
        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(5, MuxNotification::PaneAdded(5)),)
        );
        assert!(
            terminal_rx.is_empty(),
            "post-overflow established-gap ingress must not publish a second terminal reason"
        );
        assert_outbound_live_counters_zero(&coordinator);
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
            late_coordinator
                .on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)))
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
    fn topology_ingress_linearizes_before_terminal_and_rejects_every_later_event() {
        let (item_tx, item_rx) = bounded(DISPATCH_ITEM_QUEUE_TOTAL_CAPACITY);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let coordinator = Arc::new(TopologyStreamCoordinator::new(
            item_tx,
            terminal,
            TopologyStreamId::from_bytes([0x6d; 16]),
        ));

        let tripper_ready = Arc::new(std::sync::Barrier::new(2));
        let (ingress_result_tx, ingress_result_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let tripper_terminal = coordinator.terminal.clone();
        let tripper_ready_thread = Arc::clone(&tripper_ready);
        let tripper = std::thread::spawn(move || {
            tripper_ready_thread.wait();
            assert!(
                ingress_result_rx
                    .recv()
                    .expect("ingress result sender must remain connected"),
                "the topology event must win the selected linearization order"
            );
            tripper_terminal.trip(TOPOLOGY_PROTOCOL_FAILURE);
        });
        tripper_ready.wait();

        let mux = Mux::new(None);
        let accepted =
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)));
        ingress_result_tx
            .send(accepted)
            .expect("terminal tripper must remain connected");
        tripper.join().expect("terminal tripper thread should join");

        assert!(
            accepted,
            "topology ingress selected to linearize first must succeed"
        );
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("post-ingress terminal reason must publish"),
            TOPOLOGY_PROTOCOL_FAILURE
        );
        let retained = coordinator.outbound_budget.snapshot();
        assert_eq!(retained.total_slots, 1);
        assert_eq!(retained.bulk_slots, 1);
        assert_eq!(
            retained.retained_bytes,
            RETAINED_TOPOLOGY_EVENT_ACCOUNTED_FIXED_BYTES
        );
        assert!(item_rx.is_empty());

        assert!(
            !coordinator
                .on_notification(&mux, topology_envelope(2, MuxNotification::PaneAdded(2)),)
        );
        assert!(
            terminal_rx.is_empty(),
            "later topology ingress must not publish a second terminal reason"
        );
        assert!(item_rx.is_empty());
        let state = coordinator.state.lock();
        assert!(matches!(&state.phase, TopologyStreamPhase::Exhausted));
        assert_eq!(state.prebind.retained_bytes, 0);
        assert!(state.prebind.events.is_empty());
        drop(state);
        assert_outbound_live_counters_zero(&coordinator);
    }

    #[test]
    fn topology_result_admission_rejects_terminal_race_after_early_check() {
        let (coordinator, item_rx, terminal_rx, _, _) = bound_topology_coordinator();
        coordinator
            .begin_fence(48, &fenced_snapshot_request())
            .expect("begin terminal-race topology fence");
        let mux = Mux::new(None);
        assert!(
            coordinator.on_notification(&mux, topology_envelope(1, MuxNotification::PaneAdded(1)),)
        );

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

    #[derive(Debug)]
    struct AdmissionBarrierDispatchStream {
        admission: Arc<ParkingMutex<()>>,
        poll_entered: std::sync::mpsc::Sender<()>,
        release_poll: std::sync::mpsc::Receiver<()>,
        write_polls: Arc<AtomicUsize>,
        flush_polls: Arc<AtomicUsize>,
        bytes_written: usize,
    }

    impl AdmissionBarrierDispatchStream {
        fn enter_admitted_poll(&self) {
            assert!(
                self.admission.try_lock().is_none(),
                "the terminal admission gate must remain held during an immediate I/O poll"
            );
            self.poll_entered
                .send(())
                .expect("test poll observer must remain connected");
            self.release_poll
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("test must release an admitted I/O poll within five seconds");
        }
    }

    impl DispatchStream for AdmissionBarrierDispatchStream {
        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::pending())
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl AsyncRead for AdmissionBarrierDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for AdmissionBarrierDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.write_polls.fetch_add(1, Ordering::Relaxed);
            this.enter_admitted_poll();
            this.bytes_written = this.bytes_written.saturating_add(buf.len());
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.flush_polls.fetch_add(1, Ordering::Relaxed);
            this.enter_admitted_poll();
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
        flush_offsets: Vec<usize>,
        max_write_size: Option<usize>,
        fail_flush: bool,
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
        admission: Arc<ParkingMutex<()>>,
        readable: AtomicBool,
        combined_waits: AtomicUsize,
        retry_waits: AtomicUsize,
        read_polls: Arc<AtomicUsize>,
        write_polls: AtomicUsize,
        requires_transport_retry: bool,
        combined_wait_pending: bool,
        ready_side: Option<DispatchReadySide>,
        terminal_during_wait: Option<DispatchTerminal>,
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
            if self.combined_wait_pending {
                return Box::pin(std::future::pending());
            }
            Box::pin(async move {
                assert!(
                    self.admission.try_lock().is_some(),
                    "the terminal admission gate must be released before a readiness await"
                );
                self.combined_waits.fetch_add(1, Ordering::Relaxed);
                if let Some(terminal) = &self.terminal_during_wait {
                    terminal.trip(OUTBOUND_BUDGET_OVERFLOW);
                }
                let ready_side = self.ready_side.unwrap_or(DispatchReadySide::Readable);
                if ready_side == DispatchReadySide::Readable {
                    self.readable.store(true, Ordering::Release);
                }
                Ok(ready_side)
            })
        }

        fn try_readable_without_consuming(&self) -> io::Result<DispatchReadinessHint> {
            Ok(if self.readable.load(Ordering::Acquire) {
                DispatchReadinessHint::Ready
            } else {
                DispatchReadinessHint::NotReady
            })
        }

        fn pending_outbound_requires_retry(&self) -> bool {
            self.requires_transport_retry
        }

        fn wait_for_pending_outbound_retry(
            &self,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(async move {
                assert!(
                    self.admission.try_lock().is_some(),
                    "the terminal admission gate must be released before a transport retry await"
                );
                self.retry_waits.fetch_add(1, Ordering::Relaxed);
                if let Some(terminal) = &self.terminal_during_wait {
                    terminal.trip(OUTBOUND_BUDGET_OVERFLOW);
                }
                Ok(())
            })
        }
    }

    impl AsyncRead for PendingWriteThenReadableDispatchStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.read_polls.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingWriteThenReadableDispatchStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            assert!(
                self.admission.try_lock().is_none(),
                "the terminal admission gate must remain held during poll_write"
            );
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
            let written = this
                .max_write_size
                .map_or(buf.len(), |limit| buf.len().min(limit.max(1)));
            this.bytes.extend_from_slice(&buf[..written]);
            this.write_sizes.push(written);
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_flush {
                Poll::Ready(Err(io::Error::other("simulated outbound flush failure")))
            } else {
                let this = self.get_mut();
                this.flush_offsets.push(this.bytes.len());
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn queued_pong(serial: u64) -> Box<DecodedPdu> {
        Box::new(DecodedPdu {
            pdu: Pdu::Pong(Pong {}),
            serial,
        })
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedOutboundPdu {
        Pong,
        ErrorResponse,
        GetTlsCredsResponse {
            ca_cert_pem: String,
            client_cert_pem: String,
        },
    }

    impl GeneratedOutboundPdu {
        fn into_pdu(self) -> Pdu {
            match self {
                Self::Pong => Pdu::Pong(Pong {}),
                Self::ErrorResponse => Pdu::ErrorResponse(codec::ErrorResponse::backend_failure(
                    <codec::Ping as codec::PduWireIdent>::IDENT,
                )),
                Self::GetTlsCredsResponse {
                    ca_cert_pem,
                    client_cert_pem,
                } => Pdu::GetTlsCredsResponse(codec::GetTlsCredsResponse {
                    ca_cert_pem,
                    client_cert_pem,
                }),
            }
        }

        fn matches_decoded(&self, decoded: &Pdu) -> bool {
            match (self, decoded) {
                (Self::Pong, Pdu::Pong(Pong {})) => true,
                (Self::ErrorResponse, Pdu::ErrorResponse(response)) => {
                    *response
                        == codec::ErrorResponse::backend_failure(
                            <codec::Ping as codec::PduWireIdent>::IDENT,
                        )
                }
                (
                    Self::GetTlsCredsResponse {
                        ca_cert_pem,
                        client_cert_pem,
                    },
                    Pdu::GetTlsCredsResponse(codec::GetTlsCredsResponse {
                        ca_cert_pem: decoded_ca_cert_pem,
                        client_cert_pem: decoded_client_cert_pem,
                    }),
                ) => {
                    ca_cert_pem == decoded_ca_cert_pem && client_cert_pem == decoded_client_cert_pem
                }
                _ => false,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum GeneratedMuxNotification {
        Empty,
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
        let text = "[a-zA-Z0-9 _./-]{0,512}";
        prop_oneof![
            Just(GeneratedOutboundPdu::Pong),
            Just(GeneratedOutboundPdu::ErrorResponse),
            (text, text).prop_map(|(ca_cert_pem, client_cert_pem)| {
                GeneratedOutboundPdu::GetTlsCredsResponse {
                    ca_cert_pem,
                    client_cert_pem,
                }
            }),
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
                            .try_send(test_write_item(queued_pong(next_serial)))
                            .expect("queue generated Pong response");
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
                queued_pong(1),
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
                    matches!(decoded.pdu, Pdu::Pong(_)),
                    "generated dispatch test should only encode Pong responses"
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
            first_serial in 1u16..=u16::MAX,
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
            let full_budget = Arc::new(OutboundBudget::default());
            full_tx
                .try_send(Item::Readable)
                .expect("fill dispatch notification queue");
            prop_assert!(
                !queue_notification(
                    &full_tx,
                    &full_terminal,
                    &full_budget,
                    notification.clone(),
                ),
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
            let subscriber_budget = Arc::new(OutboundBudget::default());
            full_tx
                .try_send(Item::Readable)
                .expect("refill dispatch notification queue");
            mux.subscribe(move |notification| {
                full_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(
                    &full_tx,
                    &subscriber_terminal,
                    &subscriber_budget,
                    notification,
                )
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
            let closed_budget = Arc::new(OutboundBudget::default());
            drop(closed_rx);
            prop_assert!(
                !queue_notification(
                    &closed_tx,
                    &closed_terminal,
                    &closed_budget,
                    notification.clone(),
                ),
                "closed notification queue should report a dead subscription"
            );

            let mux = Mux::new(None);
            let closed_calls = Arc::new(AtomicUsize::new(0));
            let closed_calls_for_subscriber = Arc::clone(&closed_calls);
            let closed_subscriber_budget = Arc::new(OutboundBudget::default());
            mux.subscribe(move |notification| {
                closed_calls_for_subscriber.fetch_add(1, Ordering::Relaxed);
                queue_notification(
                    &closed_tx,
                    &closed_terminal,
                    &closed_subscriber_budget,
                    notification,
                )
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
            first_serial in 1u16..=u16::MAX,
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
            first_serial in 1u16..=u16::MAX,
        ) {
            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();

            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(test_write_item(queued_pong(next_serial)))
                            .expect("queue generated Pong response");
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
                queued_pong(u64::from(first_serial)),
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
            first_serial in 1u16..=u16::MAX,
            cut_seed in any::<usize>(),
        ) {
            let serial = u64::from(first_serial);
            let mut full_frame = Vec::new();
            Pdu::Pong(Pong {})
                .encode(&mut full_frame, serial)
                .expect("generated Pong response frame should encode");
            prop_assume!(full_frame.len() > 1);
            let fail_after_bytes = 1 + (cut_seed % (full_frame.len() - 1));

            let (item_tx, item_rx) = unbounded();
            let mut next_serial = 2_u64;
            let mut expected_remaining = Vec::new();
            for queued_item in queued_items {
                match queued_item {
                    QueuedDispatchItem::WritePdu => {
                        item_tx
                            .try_send(test_write_item(queued_pong(next_serial)))
                            .expect("queue generated Pong response");
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
                queued_pong(serial),
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
        let budget = Arc::new(OutboundBudget::default());
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        let err = queue_response_pdu(
            &item_tx,
            &terminal,
            &budget,
            Pdu::Pong(Pong {}),
            41,
            PduDeliveryClass::Control,
        )
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
    fn dormant_server_emission_registry_is_exact_and_leaves_request_implied_families_live() {
        let dormant = Pdu::all_wire_specs()
            .iter()
            .filter(|spec| is_dormant_server_wire_spec(spec))
            .map(|spec| spec.ident)
            .collect::<Vec<_>>();
        assert_eq!(
            dormant,
            [79, 84, 87, 89, 90, 92],
            "only the unactivated render and ordered-window server families may be frozen"
        );
        for ident in &dormant {
            let spec = Pdu::wire_spec_for_ident(*ident)
                .expect("every dormant server family must retain a wire specification");
            let role = if matches!(*ident, 87 | 89 | 92) {
                codec::PduWireRole::CorrelatedReply
            } else {
                codec::PduWireRole::Unilateral
            };
            assert!(spec.authorizes(codec::PduProducer::Server, role));
            assert!(!spec.authorizes(codec::PduProducer::Client, codec::PduWireRole::Request));
        }

        for ident in [76, 78, 82, 83] {
            let spec = Pdu::wire_spec_for_ident(ident)
                .expect("every request-implied post-v46 server family must remain registered");
            assert!(
                !is_dormant_server_wire_spec(spec),
                "request-implied or negotiated server PDU {ident} must not be frozen"
            );
        }

        assert_eq!(
            TopologyCapabilities::SERVER_SUPPORTED,
            TopologyCapabilities::FENCED_SNAPSHOT_V1
        );
        assert!(
            !TopologyCapabilities::SERVER_SUPPORTED
                .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1)
        );
        assert!(
            !TopologyCapabilities::SERVER_SUPPORTED
                .contains(TopologyCapabilities::WINDOW_REORDER_CAS_V1)
        );
        assert!(
            !TopologyCapabilities::SERVER_SUPPORTED
                .contains(TopologyCapabilities::EXACT_RENDER_DELIVERY_V1)
        );
    }

    fn dormant_window_order_event() -> Pdu {
        Pdu::WindowOrderEventV1(codec::WindowOrderEventV1 {
            protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
            stream_id: TopologyStreamId::from_bytes([0x31; 16]),
            session_incarnation: MuxSessionIncarnation::from_bytes([0x52; 16]),
            topology_revision: TopologyRevision::new(1),
            // Deliberately invalid if it ever reaches codec validation.  The
            // dormant-family authority must reject from the typed identity
            // before the payload is validated or encoded.
            windows: Vec::new(),
        })
    }

    #[test]
    fn ordered_emission_permits_enforce_exact_family_and_serial_matrix() {
        let request = ordered_snapshot_request(true);
        let stream_id = TopologyStreamId::from_bytes([0x31; 16]);
        let session_incarnation = MuxSessionIncarnation::from_bytes([0x52; 16]);
        let pdu87 = Pdu::ListPanesOrderedV1Response(ordered_unsupported_response(
            &request,
            stream_id,
            TopologyCapabilities::SERVER_SUPPORTED,
        ));
        let pdu89 = dormant_reorder_response(stream_id, session_incarnation);
        let pdu90 = dormant_window_order_event();
        let cases = [
            (
                "ordinary cannot emit correlated PDU87",
                ServerEmissionAuthority::Ordinary,
                &pdu87,
                87,
                false,
            ),
            (
                "snapshot permit emits correlated PDU87",
                ServerEmissionAuthority::OrderedSnapshotFence,
                &pdu87,
                87,
                true,
            ),
            (
                "snapshot permit rejects unilateral PDU87",
                ServerEmissionAuthority::OrderedSnapshotFence,
                &pdu87,
                0,
                false,
            ),
            (
                "stream permit cannot emit PDU87",
                ServerEmissionAuthority::OrderedStreamEvent,
                &pdu87,
                87,
                false,
            ),
            (
                "ordinary cannot emit unilateral PDU90",
                ServerEmissionAuthority::Ordinary,
                &pdu90,
                0,
                false,
            ),
            (
                "stream permit emits unilateral PDU90",
                ServerEmissionAuthority::OrderedStreamEvent,
                &pdu90,
                0,
                true,
            ),
            (
                "stream permit rejects correlated PDU90",
                ServerEmissionAuthority::OrderedStreamEvent,
                &pdu90,
                90,
                false,
            ),
            (
                "snapshot permit cannot emit PDU90",
                ServerEmissionAuthority::OrderedSnapshotFence,
                &pdu90,
                0,
                false,
            ),
            (
                "ordinary cannot emit PDU89",
                ServerEmissionAuthority::Ordinary,
                &pdu89,
                89,
                false,
            ),
            (
                "snapshot permit cannot emit PDU89",
                ServerEmissionAuthority::OrderedSnapshotFence,
                &pdu89,
                89,
                false,
            ),
            (
                "stream permit cannot emit PDU89",
                ServerEmissionAuthority::OrderedStreamEvent,
                &pdu89,
                89,
                false,
            ),
        ];

        for (label, authority, pdu, serial, expected_permitted) in cases {
            assert_eq!(
                authority.permits(pdu, serial),
                expected_permitted,
                "typed permit mismatch: {label}"
            );
            let (terminal, terminal_rx) = DispatchTerminal::channel();
            let result = validate_server_emission_authority(pdu, serial, &terminal, authority);
            assert_eq!(
                result.is_ok(),
                expected_permitted,
                "dormant-family guard mismatch: {label}; result={result:?}"
            );
            if expected_permitted {
                assert!(
                    terminal_rx.is_empty(),
                    "permitted family/serial pair must leave the connection live: {label}"
                );
            } else {
                let role = if serial == 0 {
                    codec::PduWireRole::Unilateral
                } else {
                    codec::PduWireRole::CorrelatedReply
                };
                let expected_reason = if pdu
                    .wire_spec()
                    .is_some_and(|spec| spec.authorizes(codec::PduProducer::Server, role))
                {
                    DORMANT_OUTBOUND_PROTOCOL_FAILURE
                } else {
                    OUTBOUND_WIRE_AUTHORITY_FAILURE
                };
                assert_eq!(
                    terminal_rx
                        .try_recv()
                        .expect("rejected family/serial pair must trip the connection"),
                    expected_reason,
                    "wrong terminal reason: {label}"
                );
            }
        }
    }

    #[test]
    fn server_emission_rejects_request_and_serial_role_mismatches_before_queueing() {
        let cases = [
            (
                "response emitted as unilateral",
                Pdu::Pong(Pong {}),
                0,
                "Pong",
            ),
            ("request emitted as response", Pdu::Ping(Ping {}), 1, "Ping"),
            (
                "unilateral emitted as response",
                Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 41 }),
                1,
                "PaneRemoved",
            ),
            (
                "unassigned family",
                Pdu::Invalid { ident: 5 },
                17,
                "unassigned PDU family",
            ),
        ];

        for (label, pdu, serial, error_fragment) in cases {
            let (item_tx, item_rx) = bounded(1);
            let (terminal, terminal_rx) = DispatchTerminal::channel();
            let budget = Arc::new(OutboundBudget::default());
            let error = queue_response_pdu(
                &item_tx,
                &terminal,
                &budget,
                pdu,
                serial,
                PduDeliveryClass::Control,
            )
            .expect_err("the server wire-role guard must fail closed");
            let message = format!("{error:#}");
            assert!(
                message.contains(error_fragment),
                "unexpected {label} rejection: {error:#}"
            );
            assert!(
                message.contains(&serial.to_string()),
                "{label} rejection omitted serial authority: {error:#}"
            );
            assert_eq!(
                terminal_rx
                    .try_recv()
                    .expect("wire-role rejection must trip the connection"),
                OUTBOUND_WIRE_AUTHORITY_FAILURE,
                "wrong terminal reason: {label}"
            );
            assert!(terminal_rx.is_empty(), "terminal reason must be sticky");
            assert!(item_rx.is_empty(), "{label} reached the outbound FIFO");
            assert_eq!(
                budget.snapshot(),
                OutboundBudgetState::default(),
                "{label} consumed outbound budget before rejection"
            );
        }

        for (label, pdu, serial) in [
            ("correlated response", Pdu::Pong(Pong {}), 1),
            (
                "unilateral notification",
                Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 43 }),
                0,
            ),
        ] {
            let (terminal, terminal_rx) = DispatchTerminal::channel();
            validate_server_emission_authority(
                &pdu,
                serial,
                &terminal,
                ServerEmissionAuthority::Ordinary,
            )
            .unwrap_or_else(|error| panic!("valid {label} was rejected: {error:#}"));
            assert!(
                terminal_rx.is_empty(),
                "valid {label} must leave the connection live"
            );
        }

        let budget = Arc::new(OutboundBudget::default());
        let reservation = budget
            .try_reserve(OutboundClass::Control, 0)
            .expect("reserve one synthetic encoded-frame slot");
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        encode_write_payload(
            WritePayload::Encoded(EncodedOutboundFrame {
                bytes: vec![0x02],
                reservation,
                authority: EncodedPduAuthority::capture(
                    &Pdu::Pong(Pong {}),
                    0,
                    ServerEmissionAuthority::Ordinary,
                ),
            }),
            CompressionMode::Never,
            &terminal,
        )
        .expect_err("pre-encoded serial-zero Pong must not bypass wire-role authority");
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("encoded wire-role rejection must trip the connection"),
            OUTBOUND_WIRE_AUTHORITY_FAILURE,
        );
        assert_outbound_budget_live_counters_zero(&budget);
    }

    #[test]
    fn dormant_server_emission_trips_before_queue_allocation() {
        let (item_tx, item_rx) = bounded(1);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let budget = Arc::new(OutboundBudget::default());

        let error = queue_response_pdu(
            &item_tx,
            &terminal,
            &budget,
            dormant_window_order_event(),
            0,
            PduDeliveryClass::Bulk,
        )
        .expect_err("a dormant ordered-window event must fail before enqueue");
        let message = format!("{error:#}");
        assert!(message.contains("WindowOrderEventV1"), "{message}");
        assert!(message.contains("ident 90"), "{message}");
        assert!(message.contains("serial 0"), "{message}");
        assert!(matches!(item_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(budget.snapshot(), OutboundBudgetState::default());
        assert_eq!(
            terminal_rx.try_recv().ok(),
            Some(DORMANT_OUTBOUND_PROTOCOL_FAILURE)
        );
    }

    #[test]
    fn dormant_server_emission_defense_precedes_codec_validation() {
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let error = encode_write_payload(
            test_write_payload(Box::new(DecodedPdu {
                pdu: dormant_window_order_event(),
                serial: 0,
            })),
            codec::CompressionMode::Auto,
            &terminal,
        )
        .expect_err("the final typed-write chokepoint must retain the dormant-family guard");
        let message = format!("{error:#}");
        assert!(message.contains("WindowOrderEventV1"), "{message}");
        assert!(
            !message.contains("ordered-window event must contain at least one frozen window"),
            "codec payload validation ran before wire authority: {message}"
        );
        assert_eq!(
            terminal_rx.try_recv().ok(),
            Some(DORMANT_OUTBOUND_PROTOCOL_FAILURE)
        );
    }

    #[test]
    fn unilateral_conversion_preserves_notification_before_later_response() {
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(test_write_item(Box::new(DecodedPdu {
                pdu: Pdu::Pong(Pong {}),
                serial: 77,
            })))
            .expect("queue later response");
        let mut deferred_item = None;
        let (terminal, _terminal_rx) = DispatchTerminal::channel();

        let pending = prepare_unilateral_pdu(
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id: 19 }),
            test_reservation(OutboundClass::Bulk),
            &item_rx,
            &mut deferred_item,
            &terminal,
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
    fn pending_batch_moves_first_encoded_frame_and_retains_deferred_frame_identity() {
        let budget = Arc::new(OutboundBudget::default());
        let terminal = test_terminal();
        let first_bytes = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![0x41; 56 * 1024],
        })
        .encode_frame_with_mode(1, CompressionMode::Never)
        .expect("encode first test frame");
        let second_bytes = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![0x42; 16 * 1024],
        })
        .encode_frame_with_mode(2, CompressionMode::Never)
        .expect("encode deferred test frame");
        assert!(first_bytes.len() < OUTBOUND_WRITE_QUANTUM_BYTES);
        assert!(
            first_bytes.len() + second_bytes.len() > OUTBOUND_WRITE_QUANTUM_BYTES,
            "the second frame must cross the batch byte quantum"
        );
        let first_ptr = first_bytes.as_ptr();
        let second_ptr = second_bytes.as_ptr();
        let first_reservation = budget
            .try_reserve(OutboundClass::Control, 0)
            .expect("reserve first control frame");
        let second_reservation = budget
            .try_reserve(OutboundClass::Control, 0)
            .expect("reserve second control frame");
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::WritePdu(WritePayload::Encoded(
                EncodedOutboundFrame {
                    bytes: second_bytes,
                    reservation: second_reservation,
                    authority: test_encoded_authority(2),
                },
            )))
            .expect("queue already-encoded second frame");
        let mut deferred_item = None;
        let pending = prepare_pending_outbound_batch(
            WritePayload::Encoded(EncodedOutboundFrame {
                bytes: first_bytes,
                reservation: first_reservation,
                authority: test_encoded_authority(1),
            }),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare encoded batch without copying its first frame");
        assert_eq!(pending.bytes.as_ptr(), first_ptr);
        let deferred = deferred_item
            .take()
            .expect("over-quantum encoded frame must be deferred");
        let Item::WritePdu(WritePayload::Encoded(deferred)) = deferred else {
            panic!("deferred outbound item must remain an encoded frame");
        };
        assert_eq!(deferred.bytes.as_ptr(), second_ptr);
        assert_eq!(budget.snapshot().total_slots, 2);
        drop(pending);
        drop(deferred);
        let released = budget.snapshot();
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
        assert_eq!(released.retained_bytes, 0);
    }

    #[test]
    fn topology_and_control_frames_never_share_one_accounted_batch() {
        for topology_first in [true, false] {
            let budget = Arc::new(OutboundBudget::default());
            let terminal = test_terminal();
            let mut topology_bytes =
                Vec::with_capacity(TOPOLOGY_FENCE_MAX_RETAINED_BYTES - (64 * 1024));
            topology_bytes.push(0x54);
            let topology_capacity = topology_bytes.capacity();
            let control_bytes = vec![0x43];
            let topology = EncodedOutboundFrame {
                bytes: topology_bytes,
                reservation: budget
                    .try_reserve(OutboundClass::Topology, topology_capacity)
                    .expect("near-ceiling topology frame must fit"),
                authority: test_encoded_authority(1),
            };
            let control = EncodedOutboundFrame {
                bytes: control_bytes,
                reservation: budget
                    .try_reserve(OutboundClass::Control, 0)
                    .expect("control frame must use reserved headroom"),
                authority: test_encoded_authority(2),
            };
            let (first, second) = if topology_first {
                (topology, control)
            } else {
                (control, topology)
            };
            let expected_first_ptr = first.bytes.as_ptr();
            let expected_deferred_ptr = second.bytes.as_ptr();
            let (item_tx, item_rx) = unbounded();
            item_tx
                .try_send(Item::WritePdu(WritePayload::Encoded(second)))
                .expect("queue cross-class successor");
            let mut deferred_item = None;

            let pending = prepare_pending_outbound_batch(
                WritePayload::Encoded(first),
                &item_rx,
                &mut deferred_item,
                CompressionMode::Never,
                &terminal,
            )
            .expect("cross-class successor must be deferred without reserialization");
            assert_eq!(pending.bytes.as_ptr(), expected_first_ptr);
            let Item::WritePdu(WritePayload::Encoded(deferred)) = deferred_item
                .take()
                .expect("topology/control transition must form a batch boundary")
            else {
                panic!("cross-class successor must remain an encoded write frame");
            };
            assert_eq!(deferred.bytes.as_ptr(), expected_deferred_ptr);
            assert_eq!(budget.snapshot().retained_bytes, topology_capacity);

            drop(pending);
            drop(deferred);
            let released = budget.snapshot();
            assert_eq!(released.retained_bytes, 0);
            assert_eq!(released.total_slots, 0);
            assert_eq!(released.bulk_slots, 0);
        }
    }

    #[test]
    fn same_class_retained_frames_stay_segmented_below_the_write_quantum() {
        for class in [OutboundClass::Topology, OutboundClass::Snapshot] {
            let budget = Arc::new(OutboundBudget::default());
            let terminal = test_terminal();
            let mut first_bytes = Vec::with_capacity(32);
            first_bytes.push(0x41);
            let mut second_bytes = Vec::with_capacity(48);
            second_bytes.push(0x42);
            assert!(first_bytes.len() + second_bytes.len() < OUTBOUND_WRITE_QUANTUM_BYTES);
            let first_capacity = first_bytes.capacity();
            let second_capacity = second_bytes.capacity();
            let first_ptr = first_bytes.as_ptr();
            let second_ptr = second_bytes.as_ptr();
            let first = EncodedOutboundFrame {
                bytes: first_bytes,
                reservation: budget
                    .try_reserve(class, first_capacity)
                    .expect("reserve first retained frame"),
                authority: test_encoded_authority(1),
            };
            let second = EncodedOutboundFrame {
                bytes: second_bytes,
                reservation: budget
                    .try_reserve(class, second_capacity)
                    .expect("reserve second retained frame"),
                authority: test_encoded_authority(2),
            };
            let retained_before = budget.snapshot();
            assert_eq!(
                retained_before.retained_bytes,
                first_capacity + second_capacity,
            );
            let (item_tx, item_rx) = unbounded();
            item_tx
                .try_send(Item::WritePdu(WritePayload::Encoded(second)))
                .expect("queue same-class retained successor");
            let mut deferred_item = None;

            let pending = prepare_pending_outbound_batch(
                WritePayload::Encoded(first),
                &item_rx,
                &mut deferred_item,
                CompressionMode::Never,
                &terminal,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} retained successor must stay segmented: {error:#}",
                    class.label(),
                )
            });
            assert_eq!(pending.bytes.as_ptr(), first_ptr);
            assert_eq!(pending.bytes.capacity(), first_capacity);
            let Item::WritePdu(WritePayload::Encoded(deferred)) = deferred_item
                .take()
                .expect("same-class retained successor must be deferred unchanged")
            else {
                panic!("deferred retained successor must remain encoded");
            };
            assert_eq!(deferred.bytes.as_ptr(), second_ptr);
            assert_eq!(deferred.bytes.capacity(), second_capacity);
            assert_eq!(budget.snapshot(), retained_before);

            drop(pending);
            assert_eq!(budget.snapshot().retained_bytes, second_capacity);
            drop(deferred);
            assert_eq!(budget.snapshot().retained_bytes, 0);
        }
    }

    #[test]
    fn deferred_topology_frame_keeps_identity_and_charge_until_its_own_flush() {
        let budget = Arc::new(OutboundBudget::default());
        let terminal = test_terminal();
        let first_bytes = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![0x41; 56 * 1024],
        })
        .encode_frame_with_mode(1, CompressionMode::Never)
        .expect("encode first topology frame");
        let second_bytes = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![0x42; 16 * 1024],
        })
        .encode_frame_with_mode(2, CompressionMode::Never)
        .expect("encode deferred topology frame");
        let first_capacity = first_bytes.capacity();
        let second_capacity = second_bytes.capacity();
        let first_ptr = first_bytes.as_ptr();
        let second_ptr = second_bytes.as_ptr();
        let first = EncodedOutboundFrame {
            bytes: first_bytes,
            reservation: budget
                .try_reserve(OutboundClass::Topology, first_capacity)
                .expect("reserve first topology frame"),
            authority: test_encoded_authority(1),
        };
        let second = EncodedOutboundFrame {
            bytes: second_bytes,
            reservation: budget
                .try_reserve(OutboundClass::Topology, second_capacity)
                .expect("reserve second topology frame"),
            authority: test_encoded_authority(2),
        };
        let (item_tx, item_rx) = unbounded();
        item_tx
            .try_send(Item::WritePdu(WritePayload::Encoded(second)))
            .expect("queue second topology frame");
        let mut deferred_item = None;
        let mut first_pending = prepare_pending_outbound_batch(
            WritePayload::Encoded(first),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare first topology frame");
        assert_eq!(first_pending.bytes.as_ptr(), first_ptr);
        let Item::WritePdu(WritePayload::Encoded(second)) = deferred_item
            .take()
            .expect("over-quantum topology successor must be deferred")
        else {
            panic!("deferred topology successor must remain encoded");
        };
        assert_eq!(second.bytes.as_ptr(), second_ptr);
        assert_eq!(
            budget.snapshot().retained_bytes,
            first_capacity + second_capacity
        );

        let mut stream = ChunkedDispatchStream {
            max_write_size: Some(257),
            ..ChunkedDispatchStream::default()
        };
        loop {
            match promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut first_pending,
                None,
                &terminal,
            ))
            .expect("first topology frame should make progress")
            {
                OutboundService::Progress => {}
                OutboundService::Complete => break,
                other => panic!("unexpected first-frame service outcome: {other:?}"),
            }
        }
        drop(first_pending);
        assert_eq!(budget.snapshot().retained_bytes, second_capacity);

        let mut second_pending = prepare_pending_outbound_batch(
            WritePayload::Encoded(second),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare deferred topology frame once");
        assert_eq!(second_pending.bytes.as_ptr(), second_ptr);
        loop {
            match promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut second_pending,
                None,
                &terminal,
            ))
            .expect("second topology frame should make progress")
            {
                OutboundService::Progress => {}
                OutboundService::Complete => break,
                other => panic!("unexpected second-frame service outcome: {other:?}"),
            }
        }
        drop(second_pending);
        let released = budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn terminal_before_outbound_service_performs_no_write_or_flush_poll() {
        let (_item_tx, item_rx) = unbounded();
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut deferred_item = None;
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare test outbound frame");
        let mut stream = CountingDispatchStream::default();
        terminal.trip(OUTBOUND_BUDGET_OVERFLOW);

        let writing = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("terminal service check should be explicit");
        assert_eq!(writing, OutboundService::Terminal);
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(stream.flush_calls.load(Ordering::Relaxed), 0);

        pending.phase = PendingOutboundPhase::Flushing;
        let flushing = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("terminal flush check should be explicit");
        assert_eq!(flushing, OutboundService::Terminal);
        assert_eq!(stream.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(stream.flush_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("terminal reason must be published"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn topology_charge_releases_after_hard_write_and_flush_failures() {
        let (terminal, _terminal_rx) = DispatchTerminal::channel();

        let write_budget = Arc::new(OutboundBudget::default());
        let mut write_pending = test_topology_pending(&write_budget, &terminal);
        let mut write_stream = FailingWriteDispatchStream::new(NetworkWriteFailure::Other);
        promise::spawn::block_on(service_pending_outbound(
            &mut write_stream,
            &mut write_pending,
            None,
            &terminal,
        ))
        .expect_err("hard write failure must stop outbound service");
        assert!(write_budget.snapshot().retained_bytes > 0);
        drop(write_pending);
        let write_released = write_budget.snapshot();
        assert_eq!(write_released.retained_bytes, 0);
        assert_eq!(write_released.total_slots, 0);
        assert_eq!(write_released.bulk_slots, 0);

        let flush_budget = Arc::new(OutboundBudget::default());
        let mut flush_pending = test_topology_pending(&flush_budget, &terminal);
        let mut flush_stream = ChunkedDispatchStream {
            fail_flush: true,
            ..ChunkedDispatchStream::default()
        };
        assert_eq!(
            promise::spawn::block_on(service_pending_outbound(
                &mut flush_stream,
                &mut flush_pending,
                None,
                &terminal,
            ))
            .expect("the write before a flush failure should succeed"),
            OutboundService::Progress
        );
        promise::spawn::block_on(service_pending_outbound(
            &mut flush_stream,
            &mut flush_pending,
            None,
            &terminal,
        ))
        .expect_err("flush failure must stop outbound service");
        assert!(flush_budget.snapshot().retained_bytes > 0);
        drop(flush_pending);
        let flush_released = flush_budget.snapshot();
        assert_eq!(flush_released.retained_bytes, 0);
        assert_eq!(flush_released.total_slots, 0);
        assert_eq!(flush_released.bulk_slots, 0);
    }

    #[test]
    fn cancelling_pending_outbound_wait_retains_charge_until_batch_teardown() {
        let budget = Arc::new(OutboundBudget::default());
        let (terminal, _terminal_rx) = DispatchTerminal::channel();
        let mut pending = test_topology_pending(&budget, &terminal);
        let encoded_charge = budget.snapshot().retained_bytes;
        assert!(encoded_charge > 0);
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            combined_wait_pending: true,
            ..PendingWriteThenReadableDispatchStream::default()
        };
        let mut service = Box::pin(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(service.as_mut().poll(&mut cx), Poll::Pending));
        drop(service);
        assert_eq!(
            budget.snapshot().retained_bytes,
            encoded_charge,
            "cancelling the service future must not release its caller-owned batch"
        );
        drop(pending);
        let released = budget.snapshot();
        assert_eq!(released.retained_bytes, 0);
        assert_eq!(released.total_slots, 0);
        assert_eq!(released.bulk_slots, 0);
    }

    #[test]
    fn admitted_write_linearizes_before_terminal_and_no_later_poll_begins() {
        let (_item_tx, item_rx) = unbounded();
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut deferred_item = None;
        let pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare terminal-race frame");
        let (poll_entered_tx, poll_entered_rx) = std::sync::mpsc::channel();
        let (release_poll_tx, release_poll_rx) = std::sync::mpsc::channel();
        let write_polls = Arc::new(AtomicUsize::new(0));
        let flush_polls = Arc::new(AtomicUsize::new(0));
        let stream = AdmissionBarrierDispatchStream {
            admission: Arc::clone(&terminal.admission),
            poll_entered: poll_entered_tx,
            release_poll: release_poll_rx,
            write_polls: Arc::clone(&write_polls),
            flush_polls: Arc::clone(&flush_polls),
            bytes_written: 0,
        };
        let terminal_for_service = terminal.clone();
        let service = std::thread::spawn(move || {
            let mut stream = stream;
            let mut pending = pending;
            let outcome = promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut pending,
                None,
                &terminal_for_service,
            ));
            (stream, pending, outcome)
        });

        poll_entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("admitted write poll must begin within five seconds");
        let (trip_started_tx, trip_started_rx) = std::sync::mpsc::channel();
        let (trip_done_tx, trip_done_rx) = std::sync::mpsc::channel();
        let terminal_for_trip = terminal.clone();
        let tripper = std::thread::spawn(move || {
            trip_started_tx
                .send(())
                .expect("terminal-race observer must remain connected");
            terminal_for_trip.trip(OUTBOUND_BUDGET_OVERFLOW);
            trip_done_tx
                .send(())
                .expect("terminal-race completion observer must remain connected");
        });
        trip_started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("terminal trip must start within five seconds");
        assert!(
            matches!(
                trip_done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "terminal trip must not overtake an admitted write poll"
        );
        release_poll_tx
            .send(())
            .expect("admitted write poll must remain connected");

        let (mut stream, mut pending, first_outcome) =
            service.join().expect("admitted write service should join");
        tripper.join().expect("terminal trip should join");
        trip_done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("terminal trip must finish after the admitted write poll");
        assert_eq!(
            first_outcome.expect("admitted write should complete its poll"),
            OutboundService::Progress
        );
        assert_eq!(write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(flush_polls.load(Ordering::Relaxed), 0);
        assert!(stream.bytes_written > 0);
        let bytes_after_admitted_poll = stream.bytes_written;

        let second_outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("post-terminal service should return an explicit terminal outcome");
        assert_eq!(second_outcome, OutboundService::Terminal);
        assert_eq!(write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(flush_polls.load(Ordering::Relaxed), 0);
        assert_eq!(stream.bytes_written, bytes_after_admitted_poll);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("terminal race reason must publish"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn admitted_flush_linearizes_before_terminal_and_no_later_poll_begins() {
        let (_item_tx, item_rx) = unbounded();
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut deferred_item = None;
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("prepare terminal-race flush frame");
        pending.phase = PendingOutboundPhase::Flushing;
        let (poll_entered_tx, poll_entered_rx) = std::sync::mpsc::channel();
        let (release_poll_tx, release_poll_rx) = std::sync::mpsc::channel();
        let write_polls = Arc::new(AtomicUsize::new(0));
        let flush_polls = Arc::new(AtomicUsize::new(0));
        let stream = AdmissionBarrierDispatchStream {
            admission: Arc::clone(&terminal.admission),
            poll_entered: poll_entered_tx,
            release_poll: release_poll_rx,
            write_polls: Arc::clone(&write_polls),
            flush_polls: Arc::clone(&flush_polls),
            bytes_written: 0,
        };
        let terminal_for_service = terminal.clone();
        let service = std::thread::spawn(move || {
            let mut stream = stream;
            let mut pending = pending;
            let outcome = promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut pending,
                None,
                &terminal_for_service,
            ));
            (stream, pending, outcome)
        });

        poll_entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("admitted flush poll must begin within five seconds");
        let (trip_started_tx, trip_started_rx) = std::sync::mpsc::channel();
        let (trip_done_tx, trip_done_rx) = std::sync::mpsc::channel();
        let terminal_for_trip = terminal.clone();
        let tripper = std::thread::spawn(move || {
            trip_started_tx
                .send(())
                .expect("terminal-race observer must remain connected");
            terminal_for_trip.trip(OUTBOUND_BUDGET_OVERFLOW);
            trip_done_tx
                .send(())
                .expect("terminal-race completion observer must remain connected");
        });
        trip_started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("terminal trip must start within five seconds");
        assert!(
            matches!(
                trip_done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "terminal trip must not overtake an admitted flush poll"
        );
        release_poll_tx
            .send(())
            .expect("admitted flush poll must remain connected");

        let (mut stream, mut pending, first_outcome) =
            service.join().expect("admitted flush service should join");
        tripper.join().expect("terminal trip should join");
        trip_done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("terminal trip must finish after the admitted flush poll");
        assert_eq!(
            first_outcome.expect("admitted flush should complete its poll"),
            OutboundService::Complete
        );
        assert_eq!(write_polls.load(Ordering::Relaxed), 0);
        assert_eq!(flush_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.bytes_written, 0);

        let second_outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("post-terminal service should return an explicit terminal outcome");
        assert_eq!(second_outcome, OutboundService::Terminal);
        assert_eq!(write_polls.load(Ordering::Relaxed), 0);
        assert_eq!(flush_polls.load(Ordering::Relaxed), 1);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("terminal race reason must publish"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn full_notification_queue_terminates_mux_subscriber() {
        let (item_tx, item_rx) = bounded(1);
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let budget = Arc::new(OutboundBudget::default());
        item_tx
            .try_send(Item::Readable)
            .expect("fill dispatch queue");

        assert!(
            !queue_notification(&item_tx, &terminal, &budget, MuxNotification::Empty),
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
            .try_send(test_write_item(queued_pong(2)))
            .expect("queue second Pong response");
        item_tx
            .try_send(test_notification_item(MuxNotification::Empty))
            .expect("queue notification");

        let mut deferred_item = None;
        let mut stream = CountingDispatchStream::default();
        let result = promise::spawn::block_on(write_pending_pdus(
            &mut stream,
            queued_pong(1),
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
            matches!(
                deferred_item,
                Some(Item::Notif(ReservedNotification {
                    notification: MuxNotification::Empty,
                    ..
                }))
            ),
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
            .try_send(test_notification_item(MuxNotification::Empty))
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
        assert!(matches!(
            second,
            Item::Notif(ReservedNotification {
                notification: MuxNotification::Empty,
                ..
            })
        ));
        assert!(
            prefer_read,
            "the next turn must force another inbound readiness probe",
        );
    }

    #[test]
    fn oversized_outbound_frame_yields_between_bounded_chunks_for_inbound_work() {
        let (item_tx, item_rx) = unbounded();
        let first = Box::new(DecodedPdu {
            pdu: Pdu::GetTlsCredsResponse(codec::GetTlsCredsResponse {
                ca_cert_pem: "z".repeat(OUTBOUND_WRITE_QUANTUM_BYTES * 3),
                client_cert_pem: String::new(),
            }),
            serial: 1,
        });
        let mut deferred_item = None;
        let terminal = test_terminal();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(first),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("large frame should encode");
        drop(item_tx);

        assert!(
            pending.bytes.len() > OUTBOUND_WRITE_QUANTUM_BYTES,
            "test frame must exceed one outbound service quantum",
        );
        let mut stream = ChunkedDispatchStream::default();
        let first_turn = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("first write chunk should succeed");
        assert_eq!(first_turn, OutboundService::Progress);
        assert_eq!(pending.offset, OUTBOUND_WRITE_QUANTUM_BYTES);
        assert_eq!(stream.write_sizes, vec![OUTBOUND_WRITE_QUANTUM_BYTES]);

        stream.readable.store(true, Ordering::Release);
        let offset_before_read = pending.offset;
        let inbound_turn = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("readable input should preempt the next frame chunk");
        assert_eq!(inbound_turn, OutboundService::Readable);
        assert_eq!(
            pending.offset, offset_before_read,
            "inbound service must not advance the outbound frame",
        );

        stream.readable.store(false, Ordering::Release);
        loop {
            let outcome = promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut pending,
                None,
                &terminal,
            ))
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
            pdu: Pdu::Pong(Pong {}),
            serial: 1,
        });
        let mut deferred_item = None;
        let terminal = test_terminal();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(first),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            ..PendingWriteThenReadableDispatchStream::default()
        };

        let outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("newly readable input should preempt a pending write");

        assert_eq!(outcome, OutboundService::Readable);
        assert_eq!(pending.offset, 0, "a Pending write must not consume bytes");
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 1);
        assert_eq!(stream.retry_waits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pending_transport_retry_wait_releases_terminal_admission() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let terminal = test_terminal();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            requires_transport_retry: true,
            ..PendingWriteThenReadableDispatchStream::default()
        };

        let outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("transport retry wait should preserve outbound progress");

        assert_eq!(outcome, OutboundService::Progress);
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.retry_waits.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn terminal_during_pending_readiness_wait_never_publishes_readable() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            terminal_during_wait: Some(terminal.clone()),
            ..PendingWriteThenReadableDispatchStream::default()
        };

        let outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("terminal readiness race should return an explicit outcome");

        assert_eq!(outcome, OutboundService::Terminal);
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 1);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("readiness race terminal reason must publish"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn terminal_during_pending_writable_wait_never_publishes_progress() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            ready_side: Some(DispatchReadySide::Writable),
            terminal_during_wait: Some(terminal.clone()),
            ..PendingWriteThenReadableDispatchStream::default()
        };

        let outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("terminal writable race should return an explicit outcome");

        assert_eq!(outcome, OutboundService::Terminal);
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 1);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("writable race terminal reason must publish"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn terminal_during_transport_retry_wait_never_publishes_progress() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let (terminal, terminal_rx) = DispatchTerminal::channel();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = PendingWriteThenReadableDispatchStream {
            admission: Arc::clone(&terminal.admission),
            requires_transport_retry: true,
            terminal_during_wait: Some(terminal.clone()),
            ..PendingWriteThenReadableDispatchStream::default()
        };

        let outcome = promise::spawn::block_on(service_pending_outbound(
            &mut stream,
            &mut pending,
            None,
            &terminal,
        ))
        .expect("terminal retry race should return an explicit outcome");

        assert_eq!(outcome, OutboundService::Terminal);
        assert_eq!(pending.offset, 0);
        assert_eq!(stream.write_polls.load(Ordering::Relaxed), 1);
        assert_eq!(stream.retry_waits.load(Ordering::Relaxed), 1);
        assert_eq!(stream.combined_waits.load(Ordering::Relaxed), 0);
        assert_eq!(
            terminal_rx
                .try_recv()
                .expect("retry race terminal reason must publish"),
            OUTBOUND_BUDGET_OVERFLOW
        );
    }

    #[test]
    fn unsupported_readiness_probe_classifies_combined_read_wake_without_spinning() {
        let (_item_tx, item_rx) = unbounded();
        let mut deferred_item = None;
        let terminal = test_terminal();
        let mut pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Never,
            &terminal,
        )
        .expect("Pong response frame should encode");
        let mut stream = UnsupportedReadinessPendingWriteStream::default();

        for expected_polls in 1..=2 {
            let outcome = promise::spawn::block_on(service_pending_outbound(
                &mut stream,
                &mut pending,
                None,
                &terminal,
            ))
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
                .try_send(test_write_item(queued_pong(serial)))
                .expect("queue outbound Pong response");
        }
        let mut deferred_item = None;
        let terminal = test_terminal();
        let pending = prepare_pending_outbound_batch(
            test_write_payload(queued_pong(1)),
            &item_rx,
            &mut deferred_item,
            CompressionMode::Auto,
            &terminal,
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
