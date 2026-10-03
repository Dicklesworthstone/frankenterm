//! Commit one window's tab order on the mux server over the ordered-window
//! protocol (PDUs 86-89).
//!
//! The decision logic is a set of pure functions: the pure-window check, the
//! request builder, PDU89 correlation and the one-shot conflict rebase of the
//! tab-order authority contract (§8). [`commit_window_order`] drives them over
//! an [`OrderedReorderLane`], normally a request-only [`Client`]. It never
//! touches the local mux: it returns the server order the caller must apply.

use crate::client::{Client, RpcDeliveryCertainty, RpcTransportError};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use codec::{
    DomainBindingId, ListPanesOrderedV1, ListPanesOrderedV1Outcome, ListPanesOrderedV1Response,
    OrderedPaneSnapshotV1, OrderedWindowProtocolError, OrderedWindowStateV1, RemoteTabId,
    RemoteWindowId, ReorderWindowTabsV1, ReorderWindowTabsV1Outcome, ReorderWindowTabsV1Response,
    TopologyCapabilities, TopologyStreamId, WindowOrderCommitV1, WindowOrderMutationId,
    WindowReorderDigest, WindowReorderTerminalOutcomeV1, ORDERED_WINDOW_PROTOCOL_VERSION,
};
use mux::{MuxSessionIncarnation, TopologyRevision};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// The exact capability set a reorder lane advertises and requires: fenced
/// snapshot, ordered-window stream and per-window reorder compare-and-set.
#[must_use]
pub const fn ordered_lane_capabilities() -> TopologyCapabilities {
    TopologyCapabilities::from_bits(
        TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
            | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits()
            | TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
    )
}

/// The PDU86 a lane sends for `binding`. Every refresh sends this same
/// request, which the server accepts as an identical-request refresh.
#[must_use]
pub fn ordered_list_request(binding: DomainBindingId) -> ListPanesOrderedV1 {
    ListPanesOrderedV1 {
        protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
        domain_binding_id: binding,
        supported: ordered_lane_capabilities(),
        required: ordered_lane_capabilities(),
    }
}

/// One random mutation namespace with a nonwrapping sequence. Sequences run
/// from 1 to `u64::MAX - 1`; zero and `u64::MAX` are reserved on the wire.
#[derive(Debug)]
pub struct MutationNamespace {
    namespace: [u8; 16],
    next_sequence: AtomicU64,
}

impl MutationNamespace {
    /// Draw 16 random bytes, redrawing the reserved all-zero value.
    pub fn random() -> anyhow::Result<Self> {
        loop {
            let mut namespace = [0u8; 16];
            openssl::rand::rand_bytes(&mut namespace)
                .context("draw a random window-order mutation namespace")?;
            if let Some(namespace) = Self::from_bytes(namespace) {
                return Ok(namespace);
            }
        }
    }

    /// `None` for the reserved all-zero namespace.
    #[must_use]
    pub fn from_bytes(namespace: [u8; 16]) -> Option<Self> {
        (namespace != [0; 16]).then(|| Self {
            namespace,
            next_sequence: AtomicU64::new(1),
        })
    }

    #[must_use]
    pub const fn namespace(&self) -> [u8; 16] {
        self.namespace
    }

    /// The next unused mutation id, or `None` once the sequence is used up.
    /// An exhausted namespace stays exhausted; it never wraps.
    pub fn next_id(&self) -> Option<WindowOrderMutationId> {
        self.next_sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
                (sequence < u64::MAX).then(|| sequence + 1)
            })
            .ok()
            .map(|sequence| WindowOrderMutationId::new(self.namespace, sequence))
    }

    #[cfg(test)]
    fn with_next_sequence(namespace: [u8; 16], next_sequence: u64) -> Self {
        Self {
            namespace,
            next_sequence: AtomicU64::new(next_sequence),
        }
    }
}

/// Why a tab vector is not the complete membership of one remote window.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum NotPureWindow {
    #[error("the desired tab order is empty")]
    Empty,
    #[error("remote tab {} appears more than once", .0.get())]
    DuplicateTab(RemoteTabId),
    #[error("remote tab {} is in no server window", .0.get())]
    UnknownTab(RemoteTabId),
    #[error("the tabs belong to more than one server window")]
    SpansWindows,
    #[error("the tabs are not the full membership of server window {}", .0.get())]
    MembershipDiffers(RemoteWindowId),
}

/// Find the one server window whose membership equals `desired`, as a set.
pub fn find_pure_window<'a>(
    snapshot: &'a OrderedPaneSnapshotV1,
    desired: &[RemoteTabId],
) -> Result<&'a OrderedWindowStateV1, NotPureWindow> {
    let first = *desired.first().ok_or(NotPureWindow::Empty)?;
    let mut seen = HashSet::with_capacity(desired.len());
    for tab_id in desired {
        if !seen.insert(*tab_id) {
            return Err(NotPureWindow::DuplicateTab(*tab_id));
        }
    }
    let window = window_containing(snapshot, first).ok_or(NotPureWindow::UnknownTab(first))?;
    for tab_id in desired {
        if !window.ordered_tab_ids.contains(tab_id) {
            return Err(if window_containing(snapshot, *tab_id).is_some() {
                NotPureWindow::SpansWindows
            } else {
                NotPureWindow::UnknownTab(*tab_id)
            });
        }
    }
    if window.ordered_tab_ids.len() != desired.len() {
        return Err(NotPureWindow::MembershipDiffers(window.window_id));
    }
    Ok(window)
}

fn window_containing(
    snapshot: &OrderedPaneSnapshotV1,
    tab_id: RemoteTabId,
) -> Option<&OrderedWindowStateV1> {
    snapshot
        .ordered_windows
        .iter()
        .find(|window| window.ordered_tab_ids.contains(&tab_id))
}

/// `order` with `tab_id` moved to `index`, clamped to the last position.
/// `None` when `tab_id` is not in `order`.
#[must_use]
pub fn move_tab_order(
    order: &[RemoteTabId],
    tab_id: RemoteTabId,
    index: usize,
) -> Option<Vec<RemoteTabId>> {
    let from = order.iter().position(|candidate| *candidate == tab_id)?;
    let mut moved = order.to_vec();
    let tab = moved.remove(from);
    moved.insert(index.min(moved.len()), tab);
    Some(moved)
}

/// The identity one validated PDU87 gives a lane. PDU88 must carry exactly
/// this stream and session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderedLaneSession {
    pub domain_binding_id: DomainBindingId,
    pub stream_id: TopologyStreamId,
    pub session_incarnation: MuxSessionIncarnation,
    pub negotiated: TopologyCapabilities,
}

/// Why a PDU86 refresh produced no usable snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotUnavailable {
    Contended {
        attempts: u8,
    },
    RevisionExhausted,
    /// The server lacks part of [`ordered_lane_capabilities`].
    Unsupported {
        supported: TopologyCapabilities,
    },
}

/// A validated PDU87.
#[derive(Clone, Debug, PartialEq)]
pub enum AcceptedSnapshot {
    Ready {
        session: OrderedLaneSession,
        snapshot: OrderedPaneSnapshotV1,
    },
    Unavailable(SnapshotUnavailable),
}

/// Validate one PDU87 against the exact PDU86 that asked for it.
///
/// `Err` means the response is malformed or answers another request: a
/// protocol fault, so the caller should drop the lane.
pub fn accept_ordered_snapshot(
    request: &ListPanesOrderedV1,
    response: ListPanesOrderedV1Response,
) -> Result<AcceptedSnapshot, OrderedWindowProtocolError> {
    response.validate_for_request(request)?;
    let ListPanesOrderedV1Response {
        domain_binding_id,
        negotiated,
        stream_id,
        outcome,
        ..
    } = response;
    let unavailable = match outcome {
        ListPanesOrderedV1Outcome::Snapshot(snapshot)
            if negotiated.contains(ordered_lane_capabilities()) =>
        {
            let session = OrderedLaneSession {
                domain_binding_id,
                stream_id,
                session_incarnation: snapshot.session_incarnation,
                negotiated,
            };
            return Ok(AcceptedSnapshot::Ready { session, snapshot });
        }
        ListPanesOrderedV1Outcome::Snapshot(_) => SnapshotUnavailable::Unsupported {
            supported: negotiated,
        },
        ListPanesOrderedV1Outcome::Contended { attempts, .. } => {
            SnapshotUnavailable::Contended { attempts }
        }
        ListPanesOrderedV1Outcome::RevisionExhausted => SnapshotUnavailable::RevisionExhausted,
        ListPanesOrderedV1Outcome::Unsupported { supported } => {
            SnapshotUnavailable::Unsupported { supported }
        }
    };
    Ok(AcceptedSnapshot::Unavailable(unavailable))
}

/// Build and validate one PDU88 that asks the server to put `window` in
/// `desired` order. The active tab stays the server's current active tab.
pub fn build_reorder_request(
    session: &OrderedLaneSession,
    window: &OrderedWindowStateV1,
    desired: Vec<RemoteTabId>,
    mutation_id: WindowOrderMutationId,
) -> Result<ReorderWindowTabsV1, OrderedWindowProtocolError> {
    let request = ReorderWindowTabsV1 {
        protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
        domain_binding_id: session.domain_binding_id,
        stream_id: session.stream_id,
        session_incarnation: session.session_incarnation,
        window_id: window.window_id,
        expected_order_revision: window.order_revision,
        desired_tab_ids: desired,
        desired_active_tab_id: window.active_tab_id,
        mutation_id,
        digest: WindowReorderDigest::ZERO,
    }
    .with_computed_digest();
    request.validate()?;
    Ok(request)
}

/// One correlated PDU89 decision. `Replay` is unwrapped into the decision it
/// repeats, with `replayed` set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReorderDecision {
    Applied {
        commit: WindowOrderCommitV1,
        replayed: bool,
    },
    Conflict {
        commit: WindowOrderCommitV1,
        replayed: bool,
    },
    StaleIncarnation,
    Malformed,
    Exhausted,
}

/// A PDU89 that does not answer the PDU88 it was matched to.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ReorderCorrelationError {
    #[error("PDU89 is invalid: {0}")]
    Invalid(#[from] OrderedWindowProtocolError),
    #[error("PDU89 protocol version {actual} does not match the request's {expected}")]
    ProtocolVersion { expected: u16, actual: u16 },
    #[error("PDU89 answers mutation {actual:?}, not {expected:?}")]
    MutationId {
        expected: WindowOrderMutationId,
        actual: WindowOrderMutationId,
    },
    #[error("PDU89 request digest does not match the request")]
    RequestDigest,
    #[error("PDU89 names another topology stream")]
    StreamId,
    #[error("PDU89 names another mux session incarnation")]
    SessionIncarnation,
    #[error("PDU89 commits window {}, not the requested window {}", .actual.get(), .expected.get())]
    Window {
        expected: RemoteWindowId,
        actual: RemoteWindowId,
    },
}

/// Check that `response` answers exactly `request` before acting on it.
pub fn correlate_reorder_response(
    request: &ReorderWindowTabsV1,
    response: &ReorderWindowTabsV1Response,
) -> Result<ReorderDecision, ReorderCorrelationError> {
    response.validate()?;
    if response.protocol_version != request.protocol_version {
        return Err(ReorderCorrelationError::ProtocolVersion {
            expected: request.protocol_version,
            actual: response.protocol_version,
        });
    }
    if response.mutation_id != request.mutation_id {
        return Err(ReorderCorrelationError::MutationId {
            expected: request.mutation_id,
            actual: response.mutation_id,
        });
    }
    if response.request_digest != request.digest {
        return Err(ReorderCorrelationError::RequestDigest);
    }
    if response.stream_id != request.stream_id {
        return Err(ReorderCorrelationError::StreamId);
    }
    if response.session_incarnation != request.session_incarnation {
        return Err(ReorderCorrelationError::SessionIncarnation);
    }
    let (terminal, replayed) = match &response.outcome {
        ReorderWindowTabsV1Outcome::Replay(terminal) => (terminal.clone(), true),
        ReorderWindowTabsV1Outcome::Applied(commit) => (
            WindowReorderTerminalOutcomeV1::Applied(commit.clone()),
            false,
        ),
        ReorderWindowTabsV1Outcome::Conflict(commit) => (
            WindowReorderTerminalOutcomeV1::Conflict(commit.clone()),
            false,
        ),
        ReorderWindowTabsV1Outcome::StaleIncarnation => {
            (WindowReorderTerminalOutcomeV1::StaleIncarnation, false)
        }
        ReorderWindowTabsV1Outcome::Malformed => (WindowReorderTerminalOutcomeV1::Malformed, false),
        ReorderWindowTabsV1Outcome::Exhausted => (WindowReorderTerminalOutcomeV1::Exhausted, false),
    };
    let check_window = |commit: &WindowOrderCommitV1| {
        if commit.window.window_id == request.window_id {
            Ok(())
        } else {
            Err(ReorderCorrelationError::Window {
                expected: request.window_id,
                actual: commit.window.window_id,
            })
        }
    };
    Ok(match terminal {
        WindowReorderTerminalOutcomeV1::Applied(commit) => {
            check_window(&commit)?;
            ReorderDecision::Applied { commit, replayed }
        }
        WindowReorderTerminalOutcomeV1::Conflict(commit) => {
            check_window(&commit)?;
            ReorderDecision::Conflict { commit, replayed }
        }
        WindowReorderTerminalOutcomeV1::StaleIncarnation => ReorderDecision::StaleIncarnation,
        WindowReorderTerminalOutcomeV1::Malformed => ReorderDecision::Malformed,
        WindowReorderTerminalOutcomeV1::Exhausted => ReorderDecision::Exhausted,
    })
}

/// Contract §8 rebase after a conflict.
///
/// Compares the tabs common to the pinned `base` and the server's current
/// order. If their relative order is unchanged, the conflict came from
/// membership or active-tab movement: returns `desired` without closed tabs,
/// followed by new tabs in server order. If it changed, another reorder may
/// have committed and the server wins: returns `None`. Comparing `desired`
/// with the server directly would misread the user's own reorder as a
/// concurrent writer.
#[must_use]
pub fn rebase_after_conflict(
    base: &[RemoteTabId],
    desired: &[RemoteTabId],
    server: &[RemoteTabId],
) -> Option<Vec<RemoteTabId>> {
    let base_set: HashSet<_> = base.iter().copied().collect();
    let server_set: HashSet<_> = server.iter().copied().collect();
    let base_common = base.iter().filter(|tab| server_set.contains(tab));
    let server_common = server.iter().filter(|tab| base_set.contains(tab));
    if !base_common.eq(server_common) {
        return None;
    }
    let desired_set: HashSet<_> = desired.iter().copied().collect();
    let mut rebased: Vec<_> = desired
        .iter()
        .copied()
        .filter(|tab| server_set.contains(tab))
        .collect();
    rebased.extend(
        server
            .iter()
            .copied()
            .filter(|tab| !desired_set.contains(tab)),
    );
    Some(rebased)
}

/// A connection that carries ordered-window requests. A request-only
/// [`Client`] is one; a caller that can redial wraps it and implements
/// [`OrderedReorderLane::reconnect`].
#[async_trait]
pub trait OrderedReorderLane: Send + Sync {
    async fn list_panes_ordered(
        &self,
        request: ListPanesOrderedV1,
    ) -> anyhow::Result<ListPanesOrderedV1Response>;

    async fn reorder_window_tabs(
        &self,
        request: ReorderWindowTabsV1,
    ) -> anyhow::Result<ReorderWindowTabsV1Response>;

    /// Replace a dropped connection so an unknown reorder outcome can be
    /// replayed. The default cannot redial.
    async fn reconnect(&self) -> anyhow::Result<()> {
        bail!("this ordered reorder lane cannot reconnect")
    }
}

#[async_trait]
impl OrderedReorderLane for Client {
    async fn list_panes_ordered(
        &self,
        request: ListPanesOrderedV1,
    ) -> anyhow::Result<ListPanesOrderedV1Response> {
        self.list_panes_ordered_v1(request).await
    }

    async fn reorder_window_tabs(
        &self,
        request: ReorderWindowTabsV1,
    ) -> anyhow::Result<ReorderWindowTabsV1Response> {
        self.reorder_window_tabs_v1(request).await
    }
}

/// The order a caller wants for one server window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesiredWindowOrder {
    /// The complete tab vector of one window, in the wanted order. A GUI
    /// window that mirrors one server window uses this.
    Exact(Vec<RemoteTabId>),
    /// Move one tab to `index` within its window, resolved against the
    /// fresh snapshot. `cli move-tab` uses this.
    MoveTab { tab_id: RemoteTabId, index: usize },
}

/// One request to [`commit_window_order`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowOrderIntent {
    /// Must be the binding the lane always sends in PDU86.
    pub domain_binding_id: DomainBindingId,
    /// The session the caller's ids belong to, such as the GUI main
    /// connection's pinned session. `None` accepts any session.
    pub expected_session: Option<MuxSessionIncarnation>,
    pub desired: DesiredWindowOrder,
}

/// Why the server's order stands instead of the desired one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerWinsReason {
    /// The common tabs moved relative to the pinned base: another reorder
    /// may have committed.
    ConcurrentReorder,
    /// The single automatic rebase also conflicted.
    SecondConflict,
}

/// What happened, and which server order the caller must apply locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowOrderCommitOutcome {
    /// The server committed this order. `rebased` is true when it is the
    /// §8 rebase of the desired order rather than the order itself.
    Applied {
        window: OrderedWindowStateV1,
        topology_revision: TopologyRevision,
        rebased: bool,
    },
    /// The server already had the wanted order; no PDU88 was sent, or the
    /// rebase found the server already in the rebased order.
    Unchanged {
        window: OrderedWindowStateV1,
    },
    /// The server kept its own order. Reconcile the local window to it.
    ServerWins {
        window: OrderedWindowStateV1,
        reason: ServerWinsReason,
    },
    /// The tabs are not one complete server window; nothing was sent.
    NotPureWindow(NotPureWindow),
    /// [`DesiredWindowOrder::MoveTab`] named a tab in no server window.
    UnknownTab(RemoteTabId),
    /// The lane reached another session than the caller's ids belong to.
    SessionMismatch {
        expected: MuxSessionIncarnation,
        actual: MuxSessionIncarnation,
    },
    /// The server's session changed under the request.
    StaleIncarnation,
    /// The server rejected the request twice as malformed.
    Malformed,
    /// A server revision or identity namespace cannot advance.
    Exhausted,
    /// The lane's mutation namespace is used up; make a new namespace.
    MutationIdsExhausted,
    SnapshotUnavailable(SnapshotUnavailable),
}

impl WindowOrderCommitOutcome {
    /// The server window state the caller should make the local window match,
    /// when the outcome names one.
    #[must_use]
    pub fn server_window(&self) -> Option<&OrderedWindowStateV1> {
        match self {
            Self::Applied { window, .. }
            | Self::Unchanged { window }
            | Self::ServerWins { window, .. } => Some(window),
            _ => None,
        }
    }

    /// Whether the lane's session no longer matches, so the caller should
    /// drop the lane and stop.
    #[must_use]
    pub const fn lane_is_stale(&self) -> bool {
        matches!(self, Self::StaleIncarnation | Self::SessionMismatch { .. })
    }
}

/// The lane lost the connection after PDU88 may have reached the server,
/// and the replay could not settle the outcome. The server may or may not
/// have applied the order.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("the reorder outcome is unknown: the lane dropped after sending PDU88")]
pub struct ReorderOutcomeUnknown;

enum Refreshed {
    Ready(OrderedLaneSession, OrderedPaneSnapshotV1),
    Stop(WindowOrderCommitOutcome),
}

async fn refresh<L: OrderedReorderLane + ?Sized>(
    lane: &L,
    list: &ListPanesOrderedV1,
    expected_session: Option<MuxSessionIncarnation>,
) -> anyhow::Result<Refreshed> {
    let response = lane
        .list_panes_ordered(list.clone())
        .await
        .context("refresh the ordered window snapshot")?;
    let (session, snapshot) = match accept_ordered_snapshot(list, response)
        .context("validate the ordered window snapshot")?
    {
        AcceptedSnapshot::Ready { session, snapshot } => (session, snapshot),
        AcceptedSnapshot::Unavailable(unavailable) => {
            return Ok(Refreshed::Stop(
                WindowOrderCommitOutcome::SnapshotUnavailable(unavailable),
            ))
        }
    };
    if let Some(expected) = expected_session {
        if expected != session.session_incarnation {
            return Ok(Refreshed::Stop(WindowOrderCommitOutcome::SessionMismatch {
                expected,
                actual: session.session_incarnation,
            }));
        }
    }
    Ok(Refreshed::Ready(session, snapshot))
}

fn outcome_unknown(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RpcTransportError>()
            .is_some_and(|error| error.delivery_certainty() == RpcDeliveryCertainty::OutcomeUnknown)
    })
}

/// Send one PDU88 and correlate its PDU89. If the lane drops after the
/// request may have been sent, reconnect, refresh, and resend the exact
/// request on the new stream (the digest excludes the stream), so the server
/// answers with a replay. `session` follows the new stream.
async fn send_reorder<L: OrderedReorderLane + ?Sized>(
    lane: &L,
    list: &ListPanesOrderedV1,
    session: &mut OrderedLaneSession,
    request: ReorderWindowTabsV1,
) -> anyhow::Result<ReorderDecision> {
    let error = match lane.reorder_window_tabs(request.clone()).await {
        Ok(response) => {
            return correlate_reorder_response(&request, &response)
                .context("correlate PDU89 with its PDU88")
        }
        Err(error) if outcome_unknown(&error) => error,
        Err(error) => return Err(error.context("send PDU88")),
    };
    log::warn!("reorder lane dropped after PDU88 may have been sent; replaying: {error:#}");
    lane.reconnect()
        .await
        .context(ReorderOutcomeUnknown)
        .context("reconnect the reorder lane")?;
    let response = lane
        .list_panes_ordered(list.clone())
        .await
        .context(ReorderOutcomeUnknown)?;
    let fresh = match accept_ordered_snapshot(list, response).context(ReorderOutcomeUnknown)? {
        AcceptedSnapshot::Ready { session, .. } => session,
        AcceptedSnapshot::Unavailable(unavailable) => {
            return Err(anyhow!(ReorderOutcomeUnknown)
                .context(format!("the replay refresh failed: {unavailable:?}")))
        }
    };
    if fresh.session_incarnation != request.session_incarnation {
        return Ok(ReorderDecision::StaleIncarnation);
    }
    *session = fresh;
    let mut replay = request;
    replay.stream_id = fresh.stream_id;
    replay
        .validate()
        .context("the replayed PDU88 keeps its digest on the new stream")?;
    let response = lane
        .reorder_window_tabs(replay.clone())
        .await
        .context(ReorderOutcomeUnknown)?;
    correlate_reorder_response(&replay, &response).context("correlate the replayed PDU89")
}

/// Commit `intent` on the server and return the order the caller must apply.
///
/// Each call starts with a PDU86 refresh. Then: an `Applied` result is final;
/// a `Conflict` gets at most one §8 rebase, otherwise the server wins; a
/// `Malformed` result gets one refresh and retry with a fresh mutation id;
/// `StaleIncarnation` and `Exhausted` stop. `Err` means a transport or
/// protocol fault, and [`ReorderOutcomeUnknown`] in its chain means the
/// server may have applied the order.
pub async fn commit_window_order<L: OrderedReorderLane + ?Sized>(
    lane: &L,
    namespace: &MutationNamespace,
    intent: &WindowOrderIntent,
) -> anyhow::Result<WindowOrderCommitOutcome> {
    let list = ordered_list_request(intent.domain_binding_id);
    let mut malformed_retried = false;
    'refresh: loop {
        let (mut session, snapshot) = match refresh(lane, &list, intent.expected_session).await? {
            Refreshed::Ready(session, snapshot) => (session, snapshot),
            Refreshed::Stop(outcome) => return Ok(outcome),
        };
        let desired = match &intent.desired {
            DesiredWindowOrder::Exact(order) => order.clone(),
            DesiredWindowOrder::MoveTab { tab_id, index } => {
                match window_containing(&snapshot, *tab_id)
                    .and_then(|window| move_tab_order(&window.ordered_tab_ids, *tab_id, *index))
                {
                    Some(order) => order,
                    None => return Ok(WindowOrderCommitOutcome::UnknownTab(*tab_id)),
                }
            }
        };
        let mut target = match find_pure_window(&snapshot, &desired) {
            Ok(window) => window.clone(),
            Err(reason) => return Ok(WindowOrderCommitOutcome::NotPureWindow(reason)),
        };
        if desired == target.ordered_tab_ids {
            return Ok(WindowOrderCommitOutcome::Unchanged { window: target });
        }
        let mut desired = desired;
        let mut rebased = false;
        loop {
            let Some(mutation_id) = namespace.next_id() else {
                return Ok(WindowOrderCommitOutcome::MutationIdsExhausted);
            };
            let request = build_reorder_request(&session, &target, desired.clone(), mutation_id)
                .context("build PDU88")?;
            match send_reorder(lane, &list, &mut session, request).await? {
                ReorderDecision::Applied { commit, .. } => {
                    return Ok(WindowOrderCommitOutcome::Applied {
                        window: commit.window,
                        topology_revision: commit.topology_revision,
                        rebased,
                    });
                }
                ReorderDecision::Conflict { commit, .. } if rebased => {
                    return Ok(WindowOrderCommitOutcome::ServerWins {
                        window: commit.window,
                        reason: ServerWinsReason::SecondConflict,
                    });
                }
                ReorderDecision::Conflict { commit, .. } => {
                    let server = commit.window;
                    let Some(next) = rebase_after_conflict(
                        &target.ordered_tab_ids,
                        &desired,
                        &server.ordered_tab_ids,
                    ) else {
                        return Ok(WindowOrderCommitOutcome::ServerWins {
                            window: server,
                            reason: ServerWinsReason::ConcurrentReorder,
                        });
                    };
                    if next == server.ordered_tab_ids {
                        return Ok(WindowOrderCommitOutcome::Unchanged { window: server });
                    }
                    log::debug!(
                        "rebasing window {} order onto revision {}",
                        server.window_id.get(),
                        server.order_revision.get()
                    );
                    rebased = true;
                    desired = next;
                    target = server;
                }
                ReorderDecision::Malformed if !malformed_retried => {
                    malformed_retried = true;
                    continue 'refresh;
                }
                ReorderDecision::Malformed => return Ok(WindowOrderCommitOutcome::Malformed),
                ReorderDecision::StaleIncarnation => {
                    return Ok(WindowOrderCommitOutcome::StaleIncarnation)
                }
                ReorderDecision::Exhausted => return Ok(WindowOrderCommitOutcome::Exhausted),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RpcRetirementStage;
    use codec::{ordered_pane_arena_from_list_panes, ListPanesResponse, WindowOrderRevision};
    use std::collections::{HashMap, VecDeque};
    use std::num::NonZeroU64;
    use std::sync::Mutex;

    const BINDING: DomainBindingId = DomainBindingId::from_bytes([0x71; 16]);
    const SESSION: MuxSessionIncarnation = MuxSessionIncarnation::from_bytes([0x72; 16]);

    fn tabs(ids: &[u64]) -> Vec<RemoteTabId> {
        ids.iter().copied().map(RemoteTabId::new).collect()
    }

    fn window(id: u64, revision: u64, order: &[u64], active: u64) -> OrderedWindowStateV1 {
        OrderedWindowStateV1 {
            window_id: RemoteWindowId::new(id),
            order_revision: WindowOrderRevision::new(revision),
            ordered_tab_ids: tabs(order),
            active_tab_id: Some(RemoteTabId::new(active)),
        }
    }

    fn snapshot(windows: Vec<OrderedWindowStateV1>) -> OrderedPaneSnapshotV1 {
        OrderedPaneSnapshotV1 {
            session_incarnation: SESSION,
            topology_revision: TopologyRevision::new(20),
            panes: ordered_pane_arena_from_list_panes(ListPanesResponse {
                tabs: Vec::new(),
                tab_titles: Vec::new(),
                window_titles: HashMap::new(),
                floating_panes: Vec::new(),
            })
            .expect("an empty ordered pane arena is valid"),
            floating_panes: Vec::new(),
            ordered_windows: windows,
        }
    }

    fn stream(n: u8) -> TopologyStreamId {
        TopologyStreamId::from_bytes([n; 16])
    }

    fn session(stream_id: TopologyStreamId) -> OrderedLaneSession {
        OrderedLaneSession {
            domain_binding_id: BINDING,
            stream_id,
            session_incarnation: SESSION,
            negotiated: ordered_lane_capabilities(),
        }
    }

    fn mutation(sequence: u64) -> WindowOrderMutationId {
        WindowOrderMutationId::new([0x73; 16], sequence)
    }

    fn response_for(
        request: &ReorderWindowTabsV1,
        outcome: ReorderWindowTabsV1Outcome,
    ) -> ReorderWindowTabsV1Response {
        ReorderWindowTabsV1Response {
            protocol_version: request.protocol_version,
            stream_id: request.stream_id,
            session_incarnation: request.session_incarnation,
            mutation_id: request.mutation_id,
            request_digest: request.digest,
            outcome,
        }
    }

    fn commit(state: OrderedWindowStateV1) -> WindowOrderCommitV1 {
        WindowOrderCommitV1 {
            topology_revision: TopologyRevision::new(21),
            window: state,
        }
    }

    #[test]
    fn mutation_namespace_rejects_zero_and_never_wraps() {
        assert!(MutationNamespace::from_bytes([0; 16]).is_none());
        let namespace = MutationNamespace::from_bytes([1; 16]).expect("nonzero namespace");
        assert_eq!(
            namespace.next_id(),
            Some(WindowOrderMutationId::new([1; 16], 1))
        );
        assert_eq!(
            namespace.next_id(),
            Some(WindowOrderMutationId::new([1; 16], 2))
        );

        let near_end = MutationNamespace::with_next_sequence([2; 16], u64::MAX - 1);
        assert_eq!(
            near_end.next_id(),
            Some(WindowOrderMutationId::new([2; 16], u64::MAX - 1))
        );
        assert_eq!(near_end.next_id(), None);
        assert_eq!(near_end.next_id(), None);

        let random = MutationNamespace::random().expect("draw a random namespace");
        assert_ne!(random.namespace(), [0; 16]);
    }

    #[test]
    fn pure_window_check_requires_one_complete_window() {
        let snap = snapshot(vec![
            window(1, 3, &[10, 11, 12], 10),
            window(2, 1, &[20], 20),
        ]);
        assert_eq!(
            find_pure_window(&snap, &tabs(&[12, 10, 11])).map(|w| w.window_id),
            Ok(RemoteWindowId::new(1))
        );
        assert_eq!(find_pure_window(&snap, &[]), Err(NotPureWindow::Empty));
        assert_eq!(
            find_pure_window(&snap, &tabs(&[10, 10, 11])),
            Err(NotPureWindow::DuplicateTab(RemoteTabId::new(10)))
        );
        assert_eq!(
            find_pure_window(&snap, &tabs(&[10, 11, 99])),
            Err(NotPureWindow::UnknownTab(RemoteTabId::new(99)))
        );
        assert_eq!(
            find_pure_window(&snap, &tabs(&[10, 11, 20])),
            Err(NotPureWindow::SpansWindows)
        );
        assert_eq!(
            find_pure_window(&snap, &tabs(&[11, 10])),
            Err(NotPureWindow::MembershipDiffers(RemoteWindowId::new(1)))
        );
    }

    #[test]
    fn move_tab_order_moves_and_clamps() {
        let order = tabs(&[1, 2, 3]);
        assert_eq!(
            move_tab_order(&order, RemoteTabId::new(1), 2),
            Some(tabs(&[2, 3, 1]))
        );
        assert_eq!(
            move_tab_order(&order, RemoteTabId::new(3), 0),
            Some(tabs(&[3, 1, 2]))
        );
        assert_eq!(
            move_tab_order(&order, RemoteTabId::new(2), 50),
            Some(tabs(&[1, 3, 2]))
        );
        assert_eq!(move_tab_order(&order, RemoteTabId::new(9), 0), None);
    }

    #[test]
    fn built_request_validates_and_keeps_the_server_active_tab() {
        let target = window(1, 3, &[10, 11, 12], 11);
        let request = build_reorder_request(
            &session(stream(1)),
            &target,
            tabs(&[12, 11, 10]),
            mutation(1),
        )
        .expect("a valid request");
        assert_eq!(request.expected_order_revision, target.order_revision);
        assert_eq!(request.desired_active_tab_id, target.active_tab_id);
        assert_eq!(request.digest, request.canonical_digest());
        assert_ne!(request.digest, WindowReorderDigest::ZERO);

        // The digest excludes the stream, so a replay on a new stream keeps it.
        let mut replay = request.clone();
        replay.stream_id = stream(2);
        replay.validate().expect("the replay keeps a valid digest");
        assert_eq!(replay.digest, request.digest);
    }

    #[test]
    fn correlation_accepts_only_the_exact_answer() {
        let target = window(1, 3, &[10, 11], 10);
        let request =
            build_reorder_request(&session(stream(1)), &target, tabs(&[11, 10]), mutation(4))
                .unwrap();
        let applied = commit(window(1, 4, &[11, 10], 10));

        let ok = response_for(
            &request,
            ReorderWindowTabsV1Outcome::Applied(applied.clone()),
        );
        assert_eq!(
            correlate_reorder_response(&request, &ok),
            Ok(ReorderDecision::Applied {
                commit: applied.clone(),
                replayed: false
            })
        );
        let replay = response_for(
            &request,
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Applied(
                applied.clone(),
            )),
        );
        assert_eq!(
            correlate_reorder_response(&request, &replay),
            Ok(ReorderDecision::Applied {
                commit: applied.clone(),
                replayed: true
            })
        );

        let mut wrong = ok.clone();
        wrong.mutation_id = mutation(5);
        assert!(matches!(
            correlate_reorder_response(&request, &wrong),
            Err(ReorderCorrelationError::MutationId { .. })
        ));
        let mut wrong = ok.clone();
        wrong.request_digest = WindowReorderDigest::from_bytes([9; 32]);
        assert_eq!(
            correlate_reorder_response(&request, &wrong),
            Err(ReorderCorrelationError::RequestDigest)
        );
        let mut wrong = ok.clone();
        wrong.stream_id = stream(9);
        assert_eq!(
            correlate_reorder_response(&request, &wrong),
            Err(ReorderCorrelationError::StreamId)
        );
        let mut wrong = ok.clone();
        wrong.session_incarnation = MuxSessionIncarnation::from_bytes([9; 16]);
        assert_eq!(
            correlate_reorder_response(&request, &wrong),
            Err(ReorderCorrelationError::SessionIncarnation)
        );
        let foreign = response_for(
            &request,
            ReorderWindowTabsV1Outcome::Conflict(commit(window(2, 4, &[30], 30))),
        );
        assert!(matches!(
            correlate_reorder_response(&request, &foreign),
            Err(ReorderCorrelationError::Window { .. })
        ));
        let foreign_replay = response_for(
            &request,
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Applied(commit(
                window(2, 4, &[30], 30),
            ))),
        );
        assert!(matches!(
            correlate_reorder_response(&request, &foreign_replay),
            Err(ReorderCorrelationError::Window { .. })
        ));
        assert_eq!(
            correlate_reorder_response(
                &request,
                &response_for(&request, ReorderWindowTabsV1Outcome::Malformed)
            ),
            Ok(ReorderDecision::Malformed)
        );
    }

    #[test]
    fn rebase_keeps_the_users_order_across_membership_changes() {
        // The user moved 3 to the front. Meanwhile 2 closed and 4 opened.
        let base = tabs(&[1, 2, 3]);
        let desired = tabs(&[3, 1, 2]);
        assert_eq!(
            rebase_after_conflict(&base, &desired, &tabs(&[1, 3, 4])),
            Some(tabs(&[3, 1, 4]))
        );
        // An active-tab change alone leaves the order untouched.
        assert_eq!(
            rebase_after_conflict(&base, &desired, &base),
            Some(desired.clone())
        );
    }

    #[test]
    fn rebase_refuses_after_a_concurrent_reorder() {
        let base = tabs(&[1, 2, 3]);
        let desired = tabs(&[3, 1, 2]);
        assert_eq!(
            rebase_after_conflict(&base, &desired, &tabs(&[2, 1, 3])),
            None
        );
        // Server truth equal to the user's desired order still reads as a
        // concurrent reorder against the base; the caller sees no rebase.
        assert_eq!(rebase_after_conflict(&base, &desired, &desired), None);
    }

    /// What the scripted server does with the next PDU88.
    enum Step {
        /// Evaluate normally: replay ledger, revision check, apply.
        Evaluate,
        /// Another client changes the window first, then evaluate.
        ChangeFirst(OrderedWindowStateV1),
        Malformed,
        Stale,
        Exhausted,
        /// Apply, then drop the lane before the reply reaches the client.
        ApplyAndDrop,
        /// Answer with a reply for another mutation.
        Miscorrelate,
    }

    struct ServerState {
        window: OrderedWindowStateV1,
        stream: u8,
        steps: VecDeque<Step>,
        receipts:
            HashMap<WindowOrderMutationId, (WindowReorderDigest, WindowReorderTerminalOutcomeV1)>,
        sent: Vec<ReorderWindowTabsV1>,
        lists: usize,
        reconnects: usize,
        session: MuxSessionIncarnation,
    }

    struct ScriptedLane {
        state: Mutex<ServerState>,
        can_reconnect: bool,
    }

    impl ScriptedLane {
        fn new(window: OrderedWindowStateV1, steps: Vec<Step>) -> Self {
            Self {
                state: Mutex::new(ServerState {
                    window,
                    stream: 1,
                    steps: steps.into(),
                    receipts: HashMap::new(),
                    sent: Vec::new(),
                    lists: 0,
                    reconnects: 0,
                    session: SESSION,
                }),
                can_reconnect: true,
            }
        }

        fn sent(&self) -> Vec<ReorderWindowTabsV1> {
            self.state.lock().unwrap().sent.clone()
        }
    }

    fn evaluate(
        state: &mut ServerState,
        request: &ReorderWindowTabsV1,
    ) -> WindowReorderTerminalOutcomeV1 {
        if let Some((digest, outcome)) = state.receipts.get(&request.mutation_id) {
            return if *digest == request.digest {
                outcome.clone()
            } else {
                WindowReorderTerminalOutcomeV1::Malformed
            };
        }
        let current = &state.window;
        let same_members = {
            let mut a = request.desired_tab_ids.clone();
            let mut b = current.ordered_tab_ids.clone();
            a.sort();
            b.sort();
            a == b
        };
        let outcome = if !same_members && request.expected_order_revision == current.order_revision
        {
            WindowReorderTerminalOutcomeV1::Malformed
        } else if request.expected_order_revision != current.order_revision {
            WindowReorderTerminalOutcomeV1::Conflict(commit(current.clone()))
        } else {
            state.window.ordered_tab_ids = request.desired_tab_ids.clone();
            state.window.order_revision =
                WindowOrderRevision::new(state.window.order_revision.get() + 1);
            WindowReorderTerminalOutcomeV1::Applied(commit(state.window.clone()))
        };
        state
            .receipts
            .insert(request.mutation_id, (request.digest, outcome.clone()));
        outcome
    }

    fn terminal_to_outcome(terminal: WindowReorderTerminalOutcomeV1) -> ReorderWindowTabsV1Outcome {
        match terminal {
            WindowReorderTerminalOutcomeV1::Applied(c) => ReorderWindowTabsV1Outcome::Applied(c),
            WindowReorderTerminalOutcomeV1::Conflict(c) => ReorderWindowTabsV1Outcome::Conflict(c),
            WindowReorderTerminalOutcomeV1::StaleIncarnation => {
                ReorderWindowTabsV1Outcome::StaleIncarnation
            }
            WindowReorderTerminalOutcomeV1::Malformed => ReorderWindowTabsV1Outcome::Malformed,
            WindowReorderTerminalOutcomeV1::Exhausted => ReorderWindowTabsV1Outcome::Exhausted,
        }
    }

    fn outcome_unknown_error() -> anyhow::Error {
        anyhow::Error::new(RpcTransportError::Retired {
            attempt_id: NonZeroU64::new(1).unwrap(),
            request: "ReorderWindowTabsV1",
            bound_generation: NonZeroU64::new(1).unwrap(),
            active_generation: None,
            stage: RpcRetirementStage::AwaitingResponse,
            certainty: RpcDeliveryCertainty::OutcomeUnknown,
            reason: "scripted drop".to_string(),
        })
    }

    #[async_trait]
    impl OrderedReorderLane for ScriptedLane {
        async fn list_panes_ordered(
            &self,
            request: ListPanesOrderedV1,
        ) -> anyhow::Result<ListPanesOrderedV1Response> {
            let mut state = self.state.lock().unwrap();
            state.lists += 1;
            let mut snap = snapshot(vec![state.window.clone(), window(9, 0, &[90], 90)]);
            snap.session_incarnation = state.session;
            Ok(ListPanesOrderedV1Response {
                protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                domain_binding_id: request.domain_binding_id,
                negotiated: ordered_lane_capabilities(),
                stream_id: stream(state.stream),
                outcome: ListPanesOrderedV1Outcome::Snapshot(snap),
            })
        }

        async fn reorder_window_tabs(
            &self,
            request: ReorderWindowTabsV1,
        ) -> anyhow::Result<ReorderWindowTabsV1Response> {
            let mut state = self.state.lock().unwrap();
            state.sent.push(request.clone());
            let replayed = state.receipts.contains_key(&request.mutation_id);
            let outcome = match state.steps.pop_front().unwrap_or(Step::Evaluate) {
                Step::Evaluate => evaluate(&mut state, &request),
                Step::ChangeFirst(changed) => {
                    state.window = changed;
                    evaluate(&mut state, &request)
                }
                Step::Malformed => WindowReorderTerminalOutcomeV1::Malformed,
                Step::Stale => WindowReorderTerminalOutcomeV1::StaleIncarnation,
                Step::Exhausted => WindowReorderTerminalOutcomeV1::Exhausted,
                Step::ApplyAndDrop => {
                    evaluate(&mut state, &request);
                    return Err(outcome_unknown_error());
                }
                Step::Miscorrelate => {
                    let mut response =
                        response_for(&request, ReorderWindowTabsV1Outcome::Malformed);
                    response.mutation_id = mutation(u64::MAX - 1);
                    return Ok(response);
                }
            };
            let outcome = if replayed {
                ReorderWindowTabsV1Outcome::Replay(outcome)
            } else {
                terminal_to_outcome(outcome)
            };
            Ok(response_for(&request, outcome))
        }

        async fn reconnect(&self) -> anyhow::Result<()> {
            if !self.can_reconnect {
                bail!("scripted lane cannot reconnect");
            }
            let mut state = self.state.lock().unwrap();
            state.reconnects += 1;
            state.stream += 1;
            Ok(())
        }
    }

    fn exact(order: &[u64]) -> WindowOrderIntent {
        WindowOrderIntent {
            domain_binding_id: BINDING,
            expected_session: Some(SESSION),
            desired: DesiredWindowOrder::Exact(tabs(order)),
        }
    }

    fn run(
        lane: &ScriptedLane,
        intent: &WindowOrderIntent,
    ) -> anyhow::Result<WindowOrderCommitOutcome> {
        let namespace = MutationNamespace::from_bytes([0x74; 16]).unwrap();
        futures::executor::block_on(commit_window_order(lane, &namespace, intent))
    }

    #[test]
    fn driver_applies_the_desired_order() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11, 12], 10), vec![]);
        let outcome = run(&lane, &exact(&[12, 10, 11])).unwrap();
        let WindowOrderCommitOutcome::Applied {
            window: applied,
            rebased: false,
            ..
        } = &outcome
        else {
            panic!("expected Applied, got {:?}", outcome);
        };
        assert_eq!(applied.ordered_tab_ids, tabs(&[12, 10, 11]));
        assert_eq!(outcome.server_window(), Some(applied));
        let sent = lane.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].stream_id, stream(1));
        assert_eq!(sent[0].desired_active_tab_id, Some(RemoteTabId::new(10)));
    }

    #[test]
    fn driver_sends_nothing_for_the_current_order() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![]);
        let outcome = run(&lane, &exact(&[10, 11])).unwrap();
        assert!(matches!(
            outcome,
            WindowOrderCommitOutcome::Unchanged { .. }
        ));
        assert!(lane.sent().is_empty());
    }

    #[test]
    fn driver_skips_a_mixed_window() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![]);
        let outcome = run(&lane, &exact(&[11, 10, 90])).unwrap();
        assert_eq!(
            outcome,
            WindowOrderCommitOutcome::NotPureWindow(NotPureWindow::SpansWindows)
        );
        assert!(outcome.server_window().is_none());
        assert!(lane.sent().is_empty());
    }

    #[test]
    fn driver_resolves_move_tab_against_the_fresh_snapshot() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11, 12], 10), vec![]);
        let intent = WindowOrderIntent {
            domain_binding_id: BINDING,
            expected_session: None,
            desired: DesiredWindowOrder::MoveTab {
                tab_id: RemoteTabId::new(10),
                index: 2,
            },
        };
        let outcome = run(&lane, &intent).unwrap();
        assert_eq!(
            outcome.server_window().map(|w| w.ordered_tab_ids.clone()),
            Some(tabs(&[11, 12, 10]))
        );
        let missing = WindowOrderIntent {
            desired: DesiredWindowOrder::MoveTab {
                tab_id: RemoteTabId::new(77),
                index: 0,
            },
            ..intent
        };
        assert_eq!(
            run(&lane, &missing).unwrap(),
            WindowOrderCommitOutcome::UnknownTab(RemoteTabId::new(77))
        );
    }

    #[test]
    fn driver_rebases_once_after_a_membership_conflict() {
        // Tab 13 opens on the server before our request lands.
        let lane = ScriptedLane::new(
            window(1, 3, &[10, 11, 12], 10),
            vec![Step::ChangeFirst(window(1, 4, &[10, 11, 12, 13], 10))],
        );
        let outcome = run(&lane, &exact(&[12, 10, 11])).unwrap();
        let WindowOrderCommitOutcome::Applied {
            window: applied,
            rebased: true,
            ..
        } = &outcome
        else {
            panic!("expected a rebased Applied, got {:?}", outcome);
        };
        assert_eq!(applied.ordered_tab_ids, tabs(&[12, 10, 11, 13]));
        let sent = lane.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].expected_order_revision, WindowOrderRevision::new(4));
        assert_ne!(sent[0].mutation_id, sent[1].mutation_id);
    }

    #[test]
    fn driver_lets_the_server_win_after_a_concurrent_reorder() {
        let lane = ScriptedLane::new(
            window(1, 3, &[10, 11, 12], 10),
            vec![Step::ChangeFirst(window(1, 4, &[11, 10, 12], 10))],
        );
        let outcome = run(&lane, &exact(&[12, 10, 11])).unwrap();
        assert_eq!(
            outcome,
            WindowOrderCommitOutcome::ServerWins {
                window: window(1, 4, &[11, 10, 12], 10),
                reason: ServerWinsReason::ConcurrentReorder,
            }
        );
        assert_eq!(lane.sent().len(), 1);
    }

    #[test]
    fn driver_stops_after_a_second_conflict() {
        let lane = ScriptedLane::new(
            window(1, 3, &[10, 11, 12], 10),
            vec![
                Step::ChangeFirst(window(1, 4, &[10, 11, 12, 13], 10)),
                Step::ChangeFirst(window(1, 5, &[10, 11, 12, 13, 14], 10)),
            ],
        );
        let outcome = run(&lane, &exact(&[12, 10, 11])).unwrap();
        assert!(matches!(
            outcome,
            WindowOrderCommitOutcome::ServerWins {
                reason: ServerWinsReason::SecondConflict,
                ..
            }
        ));
        assert_eq!(lane.sent().len(), 2);
    }

    #[test]
    fn driver_retries_malformed_once_with_a_fresh_snapshot() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::Malformed]);
        let outcome = run(&lane, &exact(&[11, 10])).unwrap();
        assert!(matches!(outcome, WindowOrderCommitOutcome::Applied { .. }));
        assert_eq!(lane.state.lock().unwrap().lists, 2);
        let sent = lane.sent();
        assert_eq!(sent.len(), 2);
        assert_ne!(sent[0].mutation_id, sent[1].mutation_id);

        let lane = ScriptedLane::new(
            window(1, 3, &[10, 11], 10),
            vec![Step::Malformed, Step::Malformed],
        );
        assert_eq!(
            run(&lane, &exact(&[11, 10])).unwrap(),
            WindowOrderCommitOutcome::Malformed
        );
        assert_eq!(lane.sent().len(), 2);
    }

    #[test]
    fn driver_stops_on_stale_session_and_exhaustion() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::Stale]);
        let outcome = run(&lane, &exact(&[11, 10])).unwrap();
        assert_eq!(outcome, WindowOrderCommitOutcome::StaleIncarnation);
        assert!(outcome.lane_is_stale());

        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::Exhausted]);
        assert_eq!(
            run(&lane, &exact(&[11, 10])).unwrap(),
            WindowOrderCommitOutcome::Exhausted
        );
    }

    #[test]
    fn driver_refuses_a_lane_on_another_session() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![]);
        lane.state.lock().unwrap().session = MuxSessionIncarnation::from_bytes([0x99; 16]);
        let outcome = run(&lane, &exact(&[11, 10])).unwrap();
        assert!(matches!(
            outcome,
            WindowOrderCommitOutcome::SessionMismatch { .. }
        ));
        assert!(outcome.lane_is_stale());
        assert!(lane.sent().is_empty());
    }

    #[test]
    fn driver_replays_an_unknown_outcome_on_the_new_stream() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::ApplyAndDrop]);
        let outcome = run(&lane, &exact(&[11, 10])).unwrap();
        assert!(matches!(
            outcome,
            WindowOrderCommitOutcome::Applied { rebased: false, .. }
        ));
        let sent = lane.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].mutation_id, sent[1].mutation_id);
        assert_eq!(sent[0].digest, sent[1].digest);
        assert_eq!(sent[0].stream_id, stream(1));
        assert_eq!(sent[1].stream_id, stream(2));
        let state = lane.state.lock().unwrap();
        assert_eq!(state.reconnects, 1);
        assert_eq!(state.window.order_revision, WindowOrderRevision::new(4));
    }

    #[test]
    fn driver_reports_an_unknown_outcome_it_cannot_replay() {
        let mut lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::ApplyAndDrop]);
        lane.can_reconnect = false;
        let error = run(&lane, &exact(&[11, 10])).expect_err("the outcome stays unknown");
        assert!(
            error.downcast_ref::<ReorderOutcomeUnknown>().is_some(),
            "{:#}",
            error
        );
    }

    #[test]
    fn driver_rejects_a_miscorrelated_reply() {
        let lane = ScriptedLane::new(window(1, 3, &[10, 11], 10), vec![Step::Miscorrelate]);
        let error = run(&lane, &exact(&[11, 10])).expect_err("a foreign PDU89 is a fault");
        assert!(
            matches!(
                error.downcast_ref::<ReorderCorrelationError>(),
                Some(ReorderCorrelationError::MutationId { .. })
            ),
            "{:#}",
            error
        );
    }
}
