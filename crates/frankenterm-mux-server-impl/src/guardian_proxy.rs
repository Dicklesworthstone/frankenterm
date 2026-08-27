//! Concrete mux-side proxy objects for one already-claimed guardian pane.
//!
//! This module owns the mutation-sequence actor, consuming checkpoint/output
//! replay, the resumable replay-tail reader, and the portable-pty proxy facets.
//! It still does not claim panes, choose a production guardian, rebuild the
//! window/tab topology manifest, or publish a [`LocalPane`]. The explicit
//! production selector therefore remains fail-closed: replay can return only
//! an off-topology [`ActivatedGuardianProxy`], and mux registration stays a
//! separate caller-owned commit boundary.

use frankenterm_pty_guardian::{GuardianClient, GuardianClientError};
use mux::domain::DomainId;
use mux::guardian_protocol::{
    GUARDIAN_MAX_INPUT_BYTES, GUARDIAN_MAX_PANES, GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES,
    GUARDIAN_MAX_REPLAY_RECORDS, GUARDIAN_MAX_REPLAY_WAIT_MILLIS, GuardianCensusEntry,
    GuardianCensusPaneStatus, GuardianCheckpointDescriptorV1, GuardianCheckpointIdentityDigest,
    GuardianCheckpointOutputBoundaryV1, GuardianInputEffectQuery, GuardianProtocolError,
    GuardianRejectionCode, GuardianReplayAckReceiptV1, GuardianReplayAckV1, GuardianReplayCursorV1,
    GuardianReplayDeliveryError, GuardianReplayGapReasonV1, GuardianReplayPageBodyDelivery,
    GuardianReplayPageDelivery, GuardianReplayRecordDelivery, GuardianReplayRequestV1,
    GuardianReplaySelectorV1, GuardianReply, InputEffectState,
};
use mux::localpane::{GuardianPaneLeaseControl, GuardianPaneLeaseIdentity, LocalPane};
use mux::pane::{GuardianLiveOutputReader, PaneId};
use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;
use wezterm_term::terminalstate::checkpoint::{
    TerminalCheckpointError, TerminalCheckpointLimits, TerminalCheckpointV2,
};
use wezterm_term::{InertTerminal, InertTerminalError, Terminal, TerminalConfiguration};
use zeroize::{Zeroize as _, Zeroizing};

const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_CENSUS_REFRESH_ATTEMPTS: usize = 2;
const GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS: usize = 2;
const GUARDIAN_RESTORE_REOPEN_ATTEMPTS: usize = 2;
const GUARDIAN_RESTORE_MAX_PAGES: usize = 65_536;
const GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_REPLAY_IDLE_POLL_MAX_INTERVAL: Duration = Duration::from_millis(1_000);

/// Maximum age of one guardian-scoped census snapshot used by child facets.
///
/// All panes bound to the same guardian and mux incarnation must share one
/// [`GuardianCensusCoordinator`]. That turns a polling round from one
/// paginated fleet walk per pane into one bounded fleet walk plus O(1) cache
/// lookups. Lease-changing operations explicitly invalidate the snapshot.
pub const GUARDIAN_CENSUS_CACHE_MAX_AGE: Duration = Duration::from_millis(50);

/// Every portable-pty facet for a pane shares this one mutation authority.
///
/// Keeping the mutex in the public type makes the serialization boundary
/// explicit without exposing any of the actor's mutable fields.
pub type SharedGuardianPaneLeaseActor = Arc<Mutex<GuardianPaneLeaseActor>>;

/// Content-free failures from the mux-side guardian proxy.
#[derive(Debug, Error)]
pub enum GuardianProxyError {
    #[error("invalid guardian proxy configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("guardian proxy lease identity does not match the requested operation")]
    LeaseIdentityMismatch,
    #[error("guardian proxy lease is no longer attached")]
    LeaseNotAttached,
    #[error("guardian proxy lease was fenced by a different owner or generation")]
    LeaseFenced,
    #[error("guardian proxy pane is absent from the authenticated guardian census")]
    PaneNotFound,
    #[error("guardian proxy pane is quarantined")]
    PaneQuarantined,
    #[error("guardian terminal census row omitted its exit status")]
    ChildExitStatusUnavailable,
    #[error("guardian incarnation changed while reconnecting the proxy")]
    GuardianIncarnationChanged,
    #[error("guardian replay snapshot expired; durable restore must be reopened")]
    ReplaySnapshotExpired,
    #[error("guardian replay has an authenticated output gap")]
    ReplayGap,
    #[error("guardian replay checkpoint was compacted during restore")]
    ReplayCompacted,
    #[error("guardian replay violated the consuming restore contract: {0}")]
    ReplayInvariant(&'static str),
    #[error("guardian replay exceeded a bounded restore resource")]
    ReplayCapacity,
    #[error("guardian replay protocol validation failed")]
    ReplayProtocol(#[source] GuardianProtocolError),
    #[error("guardian replay plaintext delivery failed")]
    ReplayDelivery(#[source] GuardianReplayDeliveryError),
    #[error("guardian terminal checkpoint validation failed")]
    TerminalCheckpoint(#[source] TerminalCheckpointError),
    #[error("guardian terminal suffix replay failed")]
    TerminalReplay(#[source] InertTerminalError),
    #[error("guardian terminal activation failed before topology publication")]
    TerminalActivation,
    #[error("guardian mutation outcome is indeterminate; the lease is quarantined")]
    MutationOutcomeIndeterminate,
    #[error("guardian returned a reply inconsistent with the pending mutation")]
    UnexpectedMutationReply,
    #[error("guardian input is accepted but its durable disposition is still pending")]
    InputDurabilityPending,
    #[error("guardian proved that the pending input wrote zero bytes")]
    InputKnownNotApplied,
    #[error("guardian cannot prove the disposition of the pending input")]
    InputDispositionUnavailable,
    #[error("the pending input can only be retried with its exact original bytes")]
    PendingInputPayloadRequired,
    #[error(
        "a prior guardian input was durably partial: applied {applied_bytes} of {input_bytes} bytes"
    )]
    PreviousInputPartiallyApplied {
        applied_bytes: u32,
        input_bytes: u32,
    },
    #[error("guardian input request allocation failed")]
    InputAllocation,
    #[error("guardian census cache allocation failed")]
    CensusAllocation,
    #[error("guardian mutation sequence cannot advance")]
    SequenceExhausted,
    #[error("guardian client operation failed")]
    Client(#[source] GuardianClientError),
}

impl From<GuardianProxyError> for io::Error {
    fn from(error: GuardianProxyError) -> Self {
        match error {
            GuardianProxyError::Client(GuardianClientError::Io(source)) => source,
            GuardianProxyError::LeaseFenced
            | GuardianProxyError::LeaseNotAttached
            | GuardianProxyError::PaneNotFound
            | GuardianProxyError::GuardianIncarnationChanged => {
                Self::new(io::ErrorKind::BrokenPipe, error)
            }
            GuardianProxyError::InputDurabilityPending
            | GuardianProxyError::PendingInputPayloadRequired
            | GuardianProxyError::ReplaySnapshotExpired
            | GuardianProxyError::ReplayCompacted => Self::new(io::ErrorKind::WouldBlock, error),
            GuardianProxyError::ReplayGap
            | GuardianProxyError::ReplayInvariant(_)
            | GuardianProxyError::ReplayCapacity
            | GuardianProxyError::ReplayProtocol(_)
            | GuardianProxyError::ReplayDelivery(_)
            | GuardianProxyError::TerminalCheckpoint(_)
            | GuardianProxyError::TerminalReplay(_)
            | GuardianProxyError::TerminalActivation => {
                Self::new(io::ErrorKind::InvalidData, error)
            }
            other => Self::other(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardianLeaseDisposition {
    Attached,
    TerminalObserved,
    Closed,
    Retired,
    Fenced,
    Quarantined,
    RestoreRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenericMutation {
    Resize(PtySize),
    Terminate,
    Close,
    Retire,
}

#[derive(Clone)]
struct PendingInput {
    sequence: u64,
    request_id: Uuid,
    effect_id: Uuid,
    input_bytes: u32,
    payload_sha256: [u8; 32],
    recovery_query_request_id: Option<Uuid>,
    submitted: bool,
}

impl fmt::Debug for PendingInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingInput")
            .field("sequence", &self.sequence)
            .field("request_id", &self.request_id)
            .field("effect_id", &self.effect_id)
            .field("input_bytes", &self.input_bytes)
            .field("recovery_query_request_id", &self.recovery_query_request_id)
            .field("submitted", &self.submitted)
            .finish_non_exhaustive()
    }
}

impl PendingInput {
    fn matches_payload(&self, payload: &[u8]) -> bool {
        u32::try_from(payload.len()) == Ok(self.input_bytes)
            && <[u8; 32]>::from(Sha256::digest(payload)) == self.payload_sha256
    }
}

#[derive(Clone, Debug)]
struct PendingGenericMutation {
    kind: GenericMutation,
    sequence: u64,
    request_id: Uuid,
    effect_id: Uuid,
}

#[derive(Clone, Debug)]
enum PendingMutation {
    Input(PendingInput),
    Generic(PendingGenericMutation),
}

impl PendingMutation {
    fn matches_generic(&self, kind: GenericMutation) -> bool {
        matches!(self, Self::Generic(pending) if pending.kind == kind)
    }

    fn matches_input(&self, payload: &[u8]) -> bool {
        matches!(self, Self::Input(pending) if pending.matches_payload(payload))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveredPendingMutation {
    Generic,
    InputApplied {
        applied_bytes: u32,
        input_bytes: u32,
    },
    InputKnownNotApplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedChildState {
    Running,
    Exited(i32),
}

#[derive(Debug, Error)]
enum GuardianMutationTransportError {
    #[error("guardian client failed")]
    Client(#[from] GuardianClientError),
    #[error("guardian incarnation changed")]
    GuardianIncarnationChanged,
    #[error("pane is absent")]
    PaneNotFound,
    #[error("pane lease identity changed")]
    LeaseMismatch,
    #[error("pane is quarantined")]
    PaneQuarantined,
    #[error("terminal census row omitted its exit status")]
    ChildExitStatusUnavailable,
    #[error("guardian census cache allocation failed")]
    CensusAllocation,
}

trait GuardianMutationTransport: Send {
    fn input(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        payload: Vec<u8>,
    ) -> Result<GuardianReply, GuardianMutationTransportError>;

    fn query_input_effect(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
        query: GuardianInputEffectQuery,
    ) -> Result<InputEffectState, GuardianMutationTransportError>;

    fn resize(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        size: PtySize,
    ) -> Result<GuardianReply, GuardianMutationTransportError>;

    fn terminate(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError>;

    fn close(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError>;

    fn retire(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError>;
}

/// Consuming replay transport for one exact guardian pane lease.
///
/// It is intentionally separate from the mutation actor. Replay can retain a
/// paginated snapshot and block while tailing output, while input/resize/close
/// must remain independently serialized. Plaintext pages are non-cloneable and
/// cross this boundary only by ownership transfer.
trait GuardianReplayTransport: Send {
    fn replay(
        &mut self,
        request_id: Uuid,
        request: GuardianReplayRequestV1,
    ) -> Result<GuardianReplayPageDelivery, GuardianProxyError>;

    fn replay_ack(
        &mut self,
        request_id: Uuid,
        ack: GuardianReplayAckV1,
    ) -> Result<GuardianReplayAckReceiptV1, GuardianProxyError>;
}

/// One authenticated, bounded fleet census source.
///
/// It is deliberately distinct from [`GuardianMutationTransport`]: a
/// paginated census may block on O(fleet) network work, while pane mutation
/// must remain independently serialized and responsive.
trait GuardianCensusTransport: Send {
    fn census_snapshot(
        &mut self,
    ) -> Result<Vec<GuardianCensusEntry>, GuardianMutationTransportError>;
}

struct GuardianCensusCache {
    refreshed_at: Instant,
    entries: HashMap<Uuid, GuardianCensusEntry>,
}

struct GuardianCensusCoordinatorState {
    transport: Box<dyn GuardianCensusTransport>,
    cache: Option<GuardianCensusCache>,
}

/// Explicitly shared, guardian-scoped child census coordinator.
///
/// Construct exactly one coordinator for a `(guardian_incarnation,
/// mux_incarnation)` pair and pass the same [`Arc`] to every staged pane for
/// that pair. The coordinator publishes one bounded paginated census per
/// freshness window and serves pane lookups from an immutable snapshot. A
/// snapshot evicted between pages is abandoned and reopened at most once. It
/// is neither process-global nor discovered through an implicit registry.
pub struct GuardianCensusCoordinator {
    guardian_incarnation: Uuid,
    mux_incarnation: Uuid,
    max_age: Duration,
    state: Mutex<GuardianCensusCoordinatorState>,
}

impl fmt::Debug for GuardianCensusCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        formatter
            .debug_struct("GuardianCensusCoordinator")
            .field("guardian_incarnation", &self.guardian_incarnation)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("max_age", &self.max_age)
            .field(
                "cached_entries",
                &state.cache.as_ref().map(|cache| cache.entries.len()),
            )
            .finish_non_exhaustive()
    }
}

impl GuardianCensusCoordinator {
    /// Open the sole authenticated census client for one guardian/mux pair.
    pub fn connect(
        socket_path: &Path,
        token_path: &Path,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
    ) -> Result<Self, GuardianProxyError> {
        if guardian_incarnation.is_nil() || mux_incarnation.is_nil() {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian census incarnation identities must be nonzero",
            ));
        }
        let transport = GuardianCensusClientTransport::connect(
            socket_path,
            token_path,
            guardian_incarnation,
            mux_incarnation,
        )?;
        Self::with_transport(
            guardian_incarnation,
            mux_incarnation,
            GUARDIAN_CENSUS_CACHE_MAX_AGE,
            Box::new(transport),
        )
    }

    fn with_transport(
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        max_age: Duration,
        transport: Box<dyn GuardianCensusTransport>,
    ) -> Result<Self, GuardianProxyError> {
        if guardian_incarnation.is_nil() || mux_incarnation.is_nil() {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian census incarnation identities must be nonzero",
            ));
        }
        if max_age.is_zero() {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian census freshness window must be nonzero",
            ));
        }
        Ok(Self {
            guardian_incarnation,
            mux_incarnation,
            max_age,
            state: Mutex::new(GuardianCensusCoordinatorState {
                transport,
                cache: None,
            }),
        })
    }

    fn ensure_binding(
        &self,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<(), GuardianProxyError> {
        if identity.guardian_incarnation() == self.guardian_incarnation
            && identity.mux_incarnation() == self.mux_incarnation
        {
            Ok(())
        } else {
            Err(GuardianProxyError::LeaseIdentityMismatch)
        }
    }

    /// Guardian incarnation authenticated by this coordinator.
    #[must_use]
    pub const fn guardian_incarnation(&self) -> Uuid {
        self.guardian_incarnation
    }

    /// Mux incarnation whose authenticated census connection is shared.
    #[must_use]
    pub const fn mux_incarnation(&self) -> Uuid {
        self.mux_incarnation
    }

    /// Maximum age for one immutable census snapshot.
    #[must_use]
    pub const fn max_age(&self) -> Duration {
        self.max_age
    }

    /// Invalidate the cached snapshot after any lease- or liveness-changing
    /// operation. The next observation performs one fresh bounded census.
    pub fn invalidate(&self) {
        self.state.lock().cache = None;
    }

    fn refresh_locked(
        state: &mut GuardianCensusCoordinatorState,
    ) -> Result<(), GuardianMutationTransportError> {
        state.cache = None;
        let mut entries = None;
        for attempt in 0..GUARDIAN_CENSUS_REFRESH_ATTEMPTS {
            match state.transport.census_snapshot() {
                Ok(snapshot) => {
                    entries = Some(snapshot);
                    break;
                }
                Err(GuardianMutationTransportError::Client(GuardianClientError::Rejected(
                    GuardianRejectionCode::CensusSnapshotNotFound,
                ))) if attempt + 1 < GUARDIAN_CENSUS_REFRESH_ATTEMPTS => {
                    // A bounded server-side snapshot can be evicted between
                    // pages. Abandon it and reopen once from cursor zero.
                    metrics::counter!(
                        "mux.guardian_proxy.census_snapshot_reopen_total",
                        "reason" => "snapshot_not_found",
                    )
                    .increment(1);
                }
                Err(error) => return Err(error),
            }
        }
        let Some(entries) = entries else {
            return Err(GuardianMutationTransportError::Client(
                GuardianClientError::UnexpectedReply,
            ));
        };
        if entries.len() > GUARDIAN_MAX_PANES {
            return Err(GuardianMutationTransportError::Client(
                GuardianClientError::UnexpectedReply,
            ));
        }
        let mut by_pane = HashMap::new();
        by_pane
            .try_reserve(entries.len())
            .map_err(|_| GuardianMutationTransportError::CensusAllocation)?;
        for entry in entries {
            if entry.pane_id.is_nil() || by_pane.insert(entry.pane_id, entry).is_some() {
                return Err(GuardianMutationTransportError::Client(
                    GuardianClientError::UnexpectedReply,
                ));
            }
        }
        state.cache = Some(GuardianCensusCache {
            refreshed_at: Instant::now(),
            entries: by_pane,
        });
        Ok(())
    }

    fn observe_child(
        &self,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<ObservedChildState, GuardianMutationTransportError> {
        if self.ensure_binding(identity).is_err() {
            return Err(GuardianMutationTransportError::LeaseMismatch);
        }
        let mut state = self.state.lock();
        let fresh = state.cache.as_ref().is_some_and(|cache| {
            Instant::now().saturating_duration_since(cache.refreshed_at) <= self.max_age
        });
        if !fresh {
            Self::refresh_locked(&mut state)?;
        }
        let entry = state
            .cache
            .as_ref()
            .and_then(|cache| cache.entries.get(&identity.pane_id()))
            .cloned()
            .ok_or(GuardianMutationTransportError::PaneNotFound)?;
        classify_child_census_entry(identity, entry)
    }
}

struct GuardianClientTransport {
    socket_path: PathBuf,
    token_path: PathBuf,
    identity: GuardianPaneLeaseIdentity,
    client: Option<GuardianClient>,
}

struct GuardianReplayClientTransport {
    socket_path: PathBuf,
    token_path: PathBuf,
    identity: GuardianPaneLeaseIdentity,
    client: Option<GuardianClient>,
}

impl GuardianReplayClientTransport {
    fn connect(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<Self, GuardianProxyError> {
        let mut transport = Self {
            socket_path: socket_path.to_path_buf(),
            token_path: token_path.to_path_buf(),
            identity,
            client: None,
        };
        transport.ensure_client()?;
        Ok(transport)
    }

    fn ensure_client(&mut self) -> Result<&mut GuardianClient, GuardianProxyError> {
        if self.client.is_none() {
            let client = GuardianClient::connect(
                &self.socket_path,
                &self.token_path,
                self.identity.mux_incarnation(),
            )
            .map_err(GuardianProxyError::Client)?;
            if client.guardian_incarnation() != self.identity.guardian_incarnation() {
                return Err(GuardianProxyError::GuardianIncarnationChanged);
            }
            self.client = Some(client);
        }
        self.client
            .as_mut()
            .ok_or(GuardianProxyError::GuardianIncarnationChanged)
    }

    fn call<T>(
        &mut self,
        operation: impl FnOnce(&mut GuardianClient) -> Result<T, GuardianClientError>,
    ) -> Result<T, GuardianProxyError> {
        let result = operation(self.ensure_client()?);
        if matches!(&result, Err(GuardianClientError::Io(_))) {
            // The framed stream may still contain a delayed response. Exact
            // request-ID recovery must reconnect before retrying.
            self.client = None;
        }
        result.map_err(map_replay_client_error)
    }
}

impl GuardianReplayTransport for GuardianReplayClientTransport {
    fn replay(
        &mut self,
        request_id: Uuid,
        request: GuardianReplayRequestV1,
    ) -> Result<GuardianReplayPageDelivery, GuardianProxyError> {
        let identity = self.identity;
        self.call(|client| {
            client.replay(
                identity.pane_id(),
                identity.generation(),
                request_id,
                request,
            )
        })
    }

    fn replay_ack(
        &mut self,
        request_id: Uuid,
        ack: GuardianReplayAckV1,
    ) -> Result<GuardianReplayAckReceiptV1, GuardianProxyError> {
        let identity = self.identity;
        self.call(|client| {
            client.replay_ack(identity.pane_id(), identity.generation(), request_id, ack)
        })
    }
}

fn map_replay_client_error(error: GuardianClientError) -> GuardianProxyError {
    match error {
        GuardianClientError::Rejected(GuardianRejectionCode::ReplaySnapshotExpired) => {
            GuardianProxyError::ReplaySnapshotExpired
        }
        GuardianClientError::Rejected(GuardianRejectionCode::PaneNotFound) => {
            GuardianProxyError::PaneNotFound
        }
        GuardianClientError::Rejected(GuardianRejectionCode::GuardianIncarnationMismatch) => {
            GuardianProxyError::GuardianIncarnationChanged
        }
        GuardianClientError::Rejected(
            GuardianRejectionCode::StaleLease | GuardianRejectionCode::ClaimGenerationMismatch,
        ) => GuardianProxyError::LeaseFenced,
        GuardianClientError::Rejected(GuardianRejectionCode::PaneTerminal) => {
            GuardianProxyError::LeaseNotAttached
        }
        other => GuardianProxyError::Client(other),
    }
}

impl GuardianClientTransport {
    fn connect(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<Self, GuardianProxyError> {
        let mut transport = Self {
            socket_path: socket_path.to_path_buf(),
            token_path: token_path.to_path_buf(),
            identity,
            client: None,
        };
        transport.ensure_client().map_err(|error| match error {
            GuardianMutationTransportError::Client(error) => GuardianProxyError::Client(error),
            GuardianMutationTransportError::GuardianIncarnationChanged => {
                GuardianProxyError::GuardianIncarnationChanged
            }
            GuardianMutationTransportError::PaneNotFound => GuardianProxyError::PaneNotFound,
            GuardianMutationTransportError::LeaseMismatch => GuardianProxyError::LeaseFenced,
            GuardianMutationTransportError::PaneQuarantined => GuardianProxyError::PaneQuarantined,
            GuardianMutationTransportError::ChildExitStatusUnavailable => {
                GuardianProxyError::ChildExitStatusUnavailable
            }
            GuardianMutationTransportError::CensusAllocation => {
                GuardianProxyError::CensusAllocation
            }
        })?;
        Ok(transport)
    }

    fn ensure_client(&mut self) -> Result<&mut GuardianClient, GuardianMutationTransportError> {
        if self.client.is_none() {
            let client = GuardianClient::connect(
                &self.socket_path,
                &self.token_path,
                self.identity.mux_incarnation(),
            )?;
            if client.guardian_incarnation() != self.identity.guardian_incarnation() {
                return Err(GuardianMutationTransportError::GuardianIncarnationChanged);
            }
            self.client = Some(client);
        }
        match self.client.as_mut() {
            Some(client) => Ok(client),
            None => Err(GuardianMutationTransportError::GuardianIncarnationChanged),
        }
    }

    fn call<T>(
        &mut self,
        operation: impl FnOnce(&mut GuardianClient) -> Result<T, GuardianClientError>,
    ) -> Result<T, GuardianMutationTransportError> {
        let result = operation(self.ensure_client()?);
        if matches!(&result, Err(GuardianClientError::Io(_))) {
            // A delayed response may remain on the framed stream.  Every
            // recovery attempt must start with a newly authenticated client.
            self.client = None;
        }
        result.map_err(GuardianMutationTransportError::Client)
    }
}

struct GuardianCensusClientTransport {
    socket_path: PathBuf,
    token_path: PathBuf,
    guardian_incarnation: Uuid,
    mux_incarnation: Uuid,
    client: Option<GuardianClient>,
}

impl GuardianCensusClientTransport {
    fn connect(
        socket_path: &Path,
        token_path: &Path,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
    ) -> Result<Self, GuardianProxyError> {
        let mut transport = Self {
            socket_path: socket_path.to_path_buf(),
            token_path: token_path.to_path_buf(),
            guardian_incarnation,
            mux_incarnation,
            client: None,
        };
        transport.ensure_client().map_err(|error| match error {
            GuardianMutationTransportError::Client(error) => GuardianProxyError::Client(error),
            GuardianMutationTransportError::GuardianIncarnationChanged => {
                GuardianProxyError::GuardianIncarnationChanged
            }
            GuardianMutationTransportError::PaneNotFound => GuardianProxyError::PaneNotFound,
            GuardianMutationTransportError::LeaseMismatch => GuardianProxyError::LeaseFenced,
            GuardianMutationTransportError::PaneQuarantined => GuardianProxyError::PaneQuarantined,
            GuardianMutationTransportError::ChildExitStatusUnavailable => {
                GuardianProxyError::ChildExitStatusUnavailable
            }
            GuardianMutationTransportError::CensusAllocation => {
                GuardianProxyError::CensusAllocation
            }
        })?;
        Ok(transport)
    }

    fn ensure_client(&mut self) -> Result<&mut GuardianClient, GuardianMutationTransportError> {
        if self.client.is_none() {
            let client =
                GuardianClient::connect(&self.socket_path, &self.token_path, self.mux_incarnation)?;
            if client.guardian_incarnation() != self.guardian_incarnation {
                return Err(GuardianMutationTransportError::GuardianIncarnationChanged);
            }
            self.client = Some(client);
        }
        match self.client.as_mut() {
            Some(client) => Ok(client),
            None => Err(GuardianMutationTransportError::GuardianIncarnationChanged),
        }
    }

    fn call<T>(
        &mut self,
        operation: impl FnOnce(&mut GuardianClient) -> Result<T, GuardianClientError>,
    ) -> Result<T, GuardianMutationTransportError> {
        let result = operation(self.ensure_client()?);
        if matches!(&result, Err(GuardianClientError::Io(_))) {
            self.client = None;
        }
        result.map_err(GuardianMutationTransportError::Client)
    }
}

impl GuardianCensusTransport for GuardianCensusClientTransport {
    fn census_snapshot(
        &mut self,
    ) -> Result<Vec<GuardianCensusEntry>, GuardianMutationTransportError> {
        match self.call(GuardianClient::census_snapshot) {
            Err(GuardianMutationTransportError::Client(GuardianClientError::CensusAllocation)) => {
                Err(GuardianMutationTransportError::CensusAllocation)
            }
            result => result,
        }
    }
}

impl GuardianMutationTransport for GuardianClientTransport {
    fn input(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        payload: Vec<u8>,
    ) -> Result<GuardianReply, GuardianMutationTransportError> {
        self.call(|client| {
            client.input(
                pane_id, generation, sequence, request_id, effect_id, payload,
            )
        })
    }

    fn query_input_effect(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
        query: GuardianInputEffectQuery,
    ) -> Result<InputEffectState, GuardianMutationTransportError> {
        self.call(|client| {
            client.query_input_effect(pane_id, generation, request_id, effect_id, query)
        })
    }

    fn resize(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        size: PtySize,
    ) -> Result<GuardianReply, GuardianMutationTransportError> {
        self.call(|client| {
            client.resize(pane_id, generation, sequence, request_id, effect_id, size)
        })
    }

    fn terminate(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError> {
        self.call(|client| client.terminate(pane_id, generation, sequence, request_id, effect_id))
    }

    fn close(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError> {
        self.call(|client| client.close(pane_id, generation, sequence, request_id, effect_id))
    }

    fn retire(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianMutationTransportError> {
        self.call(|client| {
            client.retire_lease(pane_id, generation, sequence, request_id, effect_id)
        })
    }
}

fn classify_child_census_entry(
    identity: GuardianPaneLeaseIdentity,
    entry: GuardianCensusEntry,
) -> Result<ObservedChildState, GuardianMutationTransportError> {
    if entry.pane_id != identity.pane_id() || entry.generation != identity.generation() {
        return Err(GuardianMutationTransportError::LeaseMismatch);
    }
    match entry.status {
        GuardianCensusPaneStatus::LiveClaimed
            if entry.mux_incarnation == Some(identity.mux_incarnation()) =>
        {
            Ok(ObservedChildState::Running)
        }
        GuardianCensusPaneStatus::ExitedUnclaimed | GuardianCensusPaneStatus::ClosedTerminal => {
            entry
                .exit_status
                .map(ObservedChildState::Exited)
                .ok_or(GuardianMutationTransportError::ChildExitStatusUnavailable)
        }
        GuardianCensusPaneStatus::Quarantined => {
            Err(GuardianMutationTransportError::PaneQuarantined)
        }
        GuardianCensusPaneStatus::LiveClaimed | GuardianCensusPaneStatus::LiveUnclaimed => {
            Err(GuardianMutationTransportError::LeaseMismatch)
        }
    }
}

/// Serialized authority for one already-claimed guardian pane.
///
/// The actor is always shared through [`SharedGuardianPaneLeaseActor`].  A
/// mutation is installed in `pending` before any fallible transport call.  An
/// I/O failure therefore preserves the exact request UUID, effect UUID, and
/// lease sequence for a fresh-connection retry.  Pending input retains only
/// its length and SHA-256 commitment; plaintext is supplied again by the
/// caller only after `QueryInputEffect` proves that resending is safe.
pub struct GuardianPaneLeaseActor {
    identity: GuardianPaneLeaseIdentity,
    next_sequence: u64,
    size: PtySize,
    disposition: GuardianLeaseDisposition,
    pending: Option<PendingMutation>,
    transport: Box<dyn GuardianMutationTransport>,
    #[cfg(test)]
    fail_next_input_copy: bool,
}

impl fmt::Debug for GuardianPaneLeaseActor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianPaneLeaseActor")
            .field("identity", &self.identity)
            .field("next_sequence", &self.next_sequence)
            .field("size", &self.size)
            .field("disposition", &self.disposition)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl GuardianPaneLeaseActor {
    fn with_transport(
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        transport: Box<dyn GuardianMutationTransport>,
    ) -> Result<Self, GuardianProxyError> {
        if next_sequence == 0 {
            return Err(GuardianProxyError::InvalidConfiguration(
                "next mutation sequence must be nonzero",
            ));
        }
        validate_pty_size(size)?;
        Ok(Self {
            identity,
            next_sequence,
            size,
            disposition: GuardianLeaseDisposition::Attached,
            pending: None,
            transport,
            #[cfg(test)]
            fail_next_input_copy: false,
        })
    }

    /// Return the immutable lease identity bound to every proxy facet.
    #[must_use]
    pub const fn identity(&self) -> GuardianPaneLeaseIdentity {
        self.identity
    }

    /// Return the next mutation sequence currently authorized by the actor.
    #[must_use]
    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    fn ensure_identity(
        &self,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<(), GuardianProxyError> {
        if identity == self.identity {
            Ok(())
        } else {
            Err(GuardianProxyError::LeaseIdentityMismatch)
        }
    }

    fn ensure_attached(&self) -> Result<(), GuardianProxyError> {
        if self.disposition == GuardianLeaseDisposition::Attached {
            Ok(())
        } else {
            Err(match self.disposition {
                GuardianLeaseDisposition::Fenced => GuardianProxyError::LeaseFenced,
                GuardianLeaseDisposition::Quarantined => GuardianProxyError::PaneQuarantined,
                GuardianLeaseDisposition::RestoreRequired => {
                    GuardianProxyError::ReplaySnapshotExpired
                }
                GuardianLeaseDisposition::Attached
                | GuardianLeaseDisposition::TerminalObserved
                | GuardianLeaseDisposition::Closed
                | GuardianLeaseDisposition::Retired => GuardianProxyError::LeaseNotAttached,
            })
        }
    }

    fn ensure_pending_recovery_permitted(&self) -> Result<(), GuardianProxyError> {
        if self.disposition == GuardianLeaseDisposition::Attached
            || (self.disposition == GuardianLeaseDisposition::TerminalObserved
                && self.pending.is_some())
        {
            Ok(())
        } else {
            self.ensure_attached()
        }
    }

    fn complete_sequence(&mut self, sequence: u64) -> Result<(), GuardianProxyError> {
        if self.next_sequence != sequence {
            self.disposition = GuardianLeaseDisposition::Quarantined;
            return Err(GuardianProxyError::UnexpectedMutationReply);
        }
        let Some(next_sequence) = sequence.checked_add(1) else {
            self.disposition = GuardianLeaseDisposition::Quarantined;
            return Err(GuardianProxyError::SequenceExhausted);
        };
        self.next_sequence = next_sequence;
        self.pending = None;
        if self.disposition == GuardianLeaseDisposition::TerminalObserved {
            self.disposition = GuardianLeaseDisposition::Closed;
        }
        Ok(())
    }

    fn transport_failure(&mut self, error: GuardianMutationTransportError) -> GuardianProxyError {
        match error {
            GuardianMutationTransportError::Client(GuardianClientError::Rejected(code)) => {
                match code {
                    GuardianRejectionCode::PaneNotFound => {
                        self.disposition = GuardianLeaseDisposition::Fenced;
                        GuardianProxyError::PaneNotFound
                    }
                    GuardianRejectionCode::GuardianIncarnationMismatch => {
                        self.disposition = GuardianLeaseDisposition::Fenced;
                        GuardianProxyError::GuardianIncarnationChanged
                    }
                    GuardianRejectionCode::StaleLease
                    | GuardianRejectionCode::ClaimGenerationMismatch => {
                        self.disposition = GuardianLeaseDisposition::Fenced;
                        GuardianProxyError::LeaseFenced
                    }
                    GuardianRejectionCode::PaneTerminal => {
                        self.disposition = GuardianLeaseDisposition::Closed;
                        GuardianProxyError::LeaseNotAttached
                    }
                    GuardianRejectionCode::InputDurabilityPending => {
                        GuardianProxyError::InputDurabilityPending
                    }
                    GuardianRejectionCode::ReplaySnapshotExpired => {
                        self.disposition = GuardianLeaseDisposition::RestoreRequired;
                        GuardianProxyError::ReplaySnapshotExpired
                    }
                    GuardianRejectionCode::CapacityExhausted
                    | GuardianRejectionCode::RequestAliasCapacityExhausted => {
                        GuardianProxyError::Client(GuardianClientError::Rejected(code))
                    }
                    GuardianRejectionCode::CheckpointOutcomeIndeterminate => {
                        self.disposition = GuardianLeaseDisposition::Quarantined;
                        GuardianProxyError::MutationOutcomeIndeterminate
                    }
                    GuardianRejectionCode::InvalidRequest
                    | GuardianRejectionCode::PaneAlreadyExists
                    | GuardianRejectionCode::RequestIdentityConflict
                    | GuardianRejectionCode::EffectIdentityConflict
                    | GuardianRejectionCode::RepeatedSequence
                    | GuardianRejectionCode::SequenceGap
                    | GuardianRejectionCode::GenerationExhausted
                    | GuardianRejectionCode::SequenceExhausted
                    | GuardianRejectionCode::InputDurabilityIdentityMismatch
                    | GuardianRejectionCode::CensusSnapshotNotFound
                    | GuardianRejectionCode::CensusSnapshotIdentityConflict
                    | GuardianRejectionCode::InvalidCensusCursor
                    | GuardianRejectionCode::InternalInvariant
                    | GuardianRejectionCode::CheckpointIdentityMismatch
                    | GuardianRejectionCode::OwnedPanesPresent
                    | GuardianRejectionCode::InputKnownNotApplied => {
                        self.disposition = GuardianLeaseDisposition::Quarantined;
                        GuardianProxyError::Client(GuardianClientError::Rejected(code))
                    }
                }
            }
            GuardianMutationTransportError::Client(error) => match error {
                GuardianClientError::Io(_) | GuardianClientError::Setup(_) => {
                    GuardianProxyError::Client(error)
                }
                GuardianClientError::CensusAllocation => GuardianProxyError::CensusAllocation,
                GuardianClientError::Protocol(_) | GuardianClientError::UnexpectedReply => {
                    self.disposition = GuardianLeaseDisposition::Quarantined;
                    GuardianProxyError::Client(error)
                }
                GuardianClientError::Rejected(code) => {
                    self.disposition = GuardianLeaseDisposition::Quarantined;
                    GuardianProxyError::Client(GuardianClientError::Rejected(code))
                }
            },
            GuardianMutationTransportError::GuardianIncarnationChanged => {
                self.disposition = GuardianLeaseDisposition::Fenced;
                GuardianProxyError::GuardianIncarnationChanged
            }
            GuardianMutationTransportError::PaneNotFound => {
                self.disposition = GuardianLeaseDisposition::Fenced;
                GuardianProxyError::PaneNotFound
            }
            GuardianMutationTransportError::LeaseMismatch => {
                self.disposition = GuardianLeaseDisposition::Fenced;
                GuardianProxyError::LeaseFenced
            }
            GuardianMutationTransportError::PaneQuarantined => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                GuardianProxyError::PaneQuarantined
            }
            GuardianMutationTransportError::ChildExitStatusUnavailable => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                GuardianProxyError::ChildExitStatusUnavailable
            }
            GuardianMutationTransportError::CensusAllocation => {
                GuardianProxyError::CensusAllocation
            }
        }
    }

    // The mutable receiver is consumed by the test-only allocation-failure
    // injector; production deliberately keeps the identical method shape.
    #[cfg_attr(
        not(test),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    fn copy_input(&mut self, payload: &[u8]) -> Result<Vec<u8>, GuardianProxyError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_input_copy) {
            return Err(GuardianProxyError::InputAllocation);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(payload.len())
            .map_err(|_| GuardianProxyError::InputAllocation)?;
        owned.extend_from_slice(payload);
        Ok(owned)
    }

    fn begin_input(&mut self, payload: &[u8]) -> Result<(), GuardianProxyError> {
        self.ensure_attached()?;
        if self.pending.is_some() {
            return Err(GuardianProxyError::UnexpectedMutationReply);
        }
        let input_bytes = u32::try_from(payload.len()).map_err(|_| {
            GuardianProxyError::InvalidConfiguration("guardian input exceeds protocol bounds")
        })?;
        self.pending = Some(PendingMutation::Input(PendingInput {
            sequence: self.next_sequence,
            request_id: Uuid::new_v4(),
            effect_id: Uuid::new_v4(),
            input_bytes,
            payload_sha256: Sha256::digest(payload).into(),
            recovery_query_request_id: None,
            submitted: false,
        }));
        Ok(())
    }

    fn begin_generic(&mut self, kind: GenericMutation) -> Result<(), GuardianProxyError> {
        self.ensure_attached()?;
        if self.pending.is_some() {
            return Err(GuardianProxyError::UnexpectedMutationReply);
        }
        self.pending = Some(PendingMutation::Generic(PendingGenericMutation {
            kind,
            sequence: self.next_sequence,
            request_id: Uuid::new_v4(),
            effect_id: Uuid::new_v4(),
        }));
        Ok(())
    }

    fn retry_pending(
        &mut self,
        input_payload: Option<&[u8]>,
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        match self.pending.clone() {
            Some(PendingMutation::Input(pending)) => self.retry_input(pending, input_payload),
            Some(PendingMutation::Generic(pending)) => self.retry_generic(pending),
            None => Err(GuardianProxyError::UnexpectedMutationReply),
        }
    }

    fn retry_input(
        &mut self,
        pending: PendingInput,
        input_payload: Option<&[u8]>,
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        if !pending.submitted {
            let payload = input_payload.ok_or(GuardianProxyError::PendingInputPayloadRequired)?;
            if !pending.matches_payload(payload) {
                return Err(GuardianProxyError::PendingInputPayloadRequired);
            }
            return self.send_pending_input(pending, payload);
        }

        let query_request_id = match self.pending.as_mut() {
            Some(PendingMutation::Input(current)) => *current
                .recovery_query_request_id
                .get_or_insert_with(Uuid::new_v4),
            Some(PendingMutation::Generic(_)) | None => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                return Err(GuardianProxyError::UnexpectedMutationReply);
            }
        };
        let query = GuardianInputEffectQuery::new(
            self.identity.mux_incarnation(),
            pending.sequence,
            pending.input_bytes,
            pending.payload_sha256,
        )
        .map_err(|_| {
            self.disposition = GuardianLeaseDisposition::Quarantined;
            GuardianProxyError::UnexpectedMutationReply
        })?;
        let state = self
            .transport
            .query_input_effect(
                self.identity.pane_id(),
                self.identity.generation(),
                query_request_id,
                pending.effect_id,
                query,
            )
            .map_err(|error| self.transport_failure(error))?;
        match state {
            InputEffectState::NotSeen => {
                let payload =
                    input_payload.ok_or(GuardianProxyError::PendingInputPayloadRequired)?;
                if !pending.matches_payload(payload) {
                    return Err(GuardianProxyError::PendingInputPayloadRequired);
                }
                self.send_pending_input(pending, payload)
            }
            InputEffectState::AcceptedNotDurable => Err(GuardianProxyError::InputDurabilityPending),
            InputEffectState::DurableFull => {
                let input_bytes = pending.input_bytes;
                self.finish_input(
                    pending,
                    RecoveredPendingMutation::InputApplied {
                        applied_bytes: input_bytes,
                        input_bytes,
                    },
                )
            }
            InputEffectState::DurablePrefix { applied_bytes } => {
                let input_bytes = pending.input_bytes;
                self.finish_input(
                    pending,
                    RecoveredPendingMutation::InputApplied {
                        applied_bytes,
                        input_bytes,
                    },
                )
            }
            InputEffectState::KnownNotApplied => {
                self.finish_input(pending, RecoveredPendingMutation::InputKnownNotApplied)
            }
            InputEffectState::DispositionUnavailable => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::InputDispositionUnavailable)
            }
        }
    }

    fn send_pending_input(
        &mut self,
        pending: PendingInput,
        payload: &[u8],
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        if !pending.matches_payload(payload) {
            return Err(GuardianProxyError::PendingInputPayloadRequired);
        }
        // Allocation failure proves that no transport call was possible. Keep
        // `submitted=false` so the caller can retry with the exact bytes
        // directly rather than performing an unnecessary effect query.
        let payload = self.copy_input(payload)?;
        match self.pending.as_mut() {
            Some(PendingMutation::Input(current)) if current.sequence == pending.sequence => {
                current.submitted = true;
            }
            Some(PendingMutation::Input(_) | PendingMutation::Generic(_)) | None => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                return Err(GuardianProxyError::UnexpectedMutationReply);
            }
        }
        let result = self.transport.input(
            self.identity.pane_id(),
            self.identity.generation(),
            pending.sequence,
            pending.request_id,
            pending.effect_id,
            payload,
        );
        match result {
            Ok(GuardianReply::InputReceipt {
                pane_id,
                generation,
                sequence,
                effect_id,
                state,
            }) if pane_id == self.identity.pane_id()
                && generation == self.identity.generation()
                && sequence == pending.sequence
                && effect_id == pending.effect_id =>
            {
                match state {
                    InputEffectState::DurableFull => {
                        let input_bytes = pending.input_bytes;
                        self.finish_input(
                            pending,
                            RecoveredPendingMutation::InputApplied {
                                applied_bytes: input_bytes,
                                input_bytes,
                            },
                        )
                    }
                    InputEffectState::DurablePrefix { applied_bytes } => {
                        let input_bytes = pending.input_bytes;
                        self.finish_input(
                            pending,
                            RecoveredPendingMutation::InputApplied {
                                applied_bytes,
                                input_bytes,
                            },
                        )
                    }
                    InputEffectState::KnownNotApplied => {
                        self.finish_input(pending, RecoveredPendingMutation::InputKnownNotApplied)
                    }
                    InputEffectState::AcceptedNotDurable => {
                        Err(GuardianProxyError::InputDurabilityPending)
                    }
                    InputEffectState::NotSeen | InputEffectState::DispositionUnavailable => {
                        self.disposition = GuardianLeaseDisposition::Quarantined;
                        Err(GuardianProxyError::UnexpectedMutationReply)
                    }
                }
            }
            Ok(GuardianReply::EffectOutcomeIndeterminate {
                pane_id,
                generation,
                sequence,
                effect_id,
            }) if pane_id == self.identity.pane_id()
                && generation == self.identity.generation()
                && sequence == pending.sequence
                && effect_id == pending.effect_id =>
            {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::MutationOutcomeIndeterminate)
            }
            Ok(_) => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::UnexpectedMutationReply)
            }
            Err(GuardianMutationTransportError::Client(GuardianClientError::Rejected(
                GuardianRejectionCode::InputKnownNotApplied,
            ))) => self.finish_input(pending, RecoveredPendingMutation::InputKnownNotApplied),
            Err(error) => Err(self.transport_failure(error)),
        }
    }

    fn finish_input(
        &mut self,
        pending: PendingInput,
        completion: RecoveredPendingMutation,
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        self.complete_sequence(pending.sequence)?;
        Ok(completion)
    }

    fn retry_generic(
        &mut self,
        pending: PendingGenericMutation,
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        let result = match pending.kind {
            GenericMutation::Resize(size) => self.transport.resize(
                self.identity.pane_id(),
                self.identity.generation(),
                pending.sequence,
                pending.request_id,
                pending.effect_id,
                size,
            ),
            GenericMutation::Terminate => self.transport.terminate(
                self.identity.pane_id(),
                self.identity.generation(),
                pending.sequence,
                pending.request_id,
                pending.effect_id,
            ),
            GenericMutation::Close => self.transport.close(
                self.identity.pane_id(),
                self.identity.generation(),
                pending.sequence,
                pending.request_id,
                pending.effect_id,
            ),
            GenericMutation::Retire => self.transport.retire(
                self.identity.pane_id(),
                self.identity.generation(),
                pending.sequence,
                pending.request_id,
                pending.effect_id,
            ),
        };
        match result {
            Ok(GuardianReply::MutationApplied {
                pane_id,
                generation,
                sequence,
            }) if !matches!(pending.kind, GenericMutation::Retire)
                && pane_id == self.identity.pane_id()
                && generation == self.identity.generation()
                && sequence == pending.sequence =>
            {
                self.complete_sequence(pending.sequence)?;
                match pending.kind {
                    GenericMutation::Resize(size) => self.size = size,
                    GenericMutation::Close => self.disposition = GuardianLeaseDisposition::Closed,
                    GenericMutation::Terminate | GenericMutation::Retire => {}
                }
                Ok(RecoveredPendingMutation::Generic)
            }
            Ok(GuardianReply::LeaseRetired {
                pane_id,
                generation,
            }) if pending.kind == GenericMutation::Retire
                && pane_id == self.identity.pane_id()
                && generation == self.identity.generation() =>
            {
                self.complete_sequence(pending.sequence)?;
                self.disposition = GuardianLeaseDisposition::Retired;
                Ok(RecoveredPendingMutation::Generic)
            }
            Ok(GuardianReply::EffectOutcomeIndeterminate {
                pane_id,
                generation,
                sequence,
                effect_id,
            }) if pane_id == self.identity.pane_id()
                && generation == self.identity.generation()
                && sequence == pending.sequence
                && effect_id == pending.effect_id =>
            {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::MutationOutcomeIndeterminate)
            }
            Ok(_) => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::UnexpectedMutationReply)
            }
            Err(error) => Err(self.transport_failure(error)),
        }
    }

    fn reconcile_before_new_operation(
        &mut self,
        input_payload: Option<&[u8]>,
    ) -> Result<Option<RecoveredPendingMutation>, GuardianProxyError> {
        if self.pending.is_none() {
            return Ok(None);
        }
        // A terminal transport classification leaves the exact pending record
        // intact for diagnostics, but it must never become an endless retry
        // loop. A terminal census may still reconcile an operation that was
        // already submitted; it may never authorize a new mutation.
        self.ensure_pending_recovery_permitted()?;
        self.retry_pending(input_payload).map(Some)
    }

    fn write_input(&mut self, payload: &[u8]) -> Result<usize, GuardianProxyError> {
        if payload.is_empty() {
            return Ok(0);
        }
        let payload = &payload[..payload.len().min(GUARDIAN_MAX_INPUT_BYTES)];
        if self.pending.is_some() {
            let same_input = self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.matches_input(payload));
            let recovered = self.reconcile_before_new_operation(same_input.then_some(payload))?;
            match recovered {
                Some(RecoveredPendingMutation::InputApplied {
                    applied_bytes,
                    input_bytes: _,
                }) if same_input => {
                    return usize::try_from(applied_bytes)
                        .map_err(|_| GuardianProxyError::UnexpectedMutationReply);
                }
                Some(RecoveredPendingMutation::InputApplied {
                    applied_bytes,
                    input_bytes,
                }) if applied_bytes != input_bytes => {
                    return Err(GuardianProxyError::PreviousInputPartiallyApplied {
                        applied_bytes,
                        input_bytes,
                    });
                }
                Some(RecoveredPendingMutation::InputKnownNotApplied) if same_input => {
                    return Err(GuardianProxyError::InputKnownNotApplied);
                }
                Some(
                    RecoveredPendingMutation::Generic
                    | RecoveredPendingMutation::InputApplied { .. }
                    | RecoveredPendingMutation::InputKnownNotApplied,
                )
                | None => {}
            }
        }
        self.begin_input(payload)?;
        match self.retry_pending(Some(payload))? {
            RecoveredPendingMutation::InputApplied { applied_bytes, .. } => {
                usize::try_from(applied_bytes)
                    .map_err(|_| GuardianProxyError::UnexpectedMutationReply)
            }
            RecoveredPendingMutation::InputKnownNotApplied => {
                Err(GuardianProxyError::InputKnownNotApplied)
            }
            RecoveredPendingMutation::Generic => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::UnexpectedMutationReply)
            }
        }
    }

    fn flush_pending(&mut self) -> Result<(), GuardianProxyError> {
        self.ensure_pending_recovery_permitted()?;
        let Some(recovered) = self.reconcile_before_new_operation(None)? else {
            return Ok(());
        };
        match recovered {
            RecoveredPendingMutation::Generic => Ok(()),
            RecoveredPendingMutation::InputApplied {
                applied_bytes,
                input_bytes,
            } if applied_bytes == input_bytes => Ok(()),
            RecoveredPendingMutation::InputApplied {
                applied_bytes,
                input_bytes,
            } => Err(GuardianProxyError::PreviousInputPartiallyApplied {
                applied_bytes,
                input_bytes,
            }),
            RecoveredPendingMutation::InputKnownNotApplied => {
                Err(GuardianProxyError::InputKnownNotApplied)
            }
        }
    }

    fn mutate_generic(&mut self, kind: GenericMutation) -> Result<(), GuardianProxyError> {
        if (kind == GenericMutation::Close && self.disposition == GuardianLeaseDisposition::Closed)
            || (kind == GenericMutation::Retire
                && matches!(
                    self.disposition,
                    GuardianLeaseDisposition::Closed | GuardianLeaseDisposition::Retired
                ))
        {
            return Ok(());
        }
        if self.pending.is_some() {
            let same_mutation = self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.matches_generic(kind));
            let recovered = self.reconcile_before_new_operation(None)?;
            match recovered {
                Some(RecoveredPendingMutation::Generic) if same_mutation => return Ok(()),
                Some(RecoveredPendingMutation::InputApplied {
                    applied_bytes,
                    input_bytes,
                }) if applied_bytes != input_bytes => {
                    return Err(GuardianProxyError::PreviousInputPartiallyApplied {
                        applied_bytes,
                        input_bytes,
                    });
                }
                Some(
                    RecoveredPendingMutation::Generic
                    | RecoveredPendingMutation::InputApplied { .. }
                    | RecoveredPendingMutation::InputKnownNotApplied,
                )
                | None => {}
            }
        }
        self.begin_generic(kind)?;
        match self.retry_pending(None)? {
            RecoveredPendingMutation::Generic => Ok(()),
            RecoveredPendingMutation::InputApplied { .. }
            | RecoveredPendingMutation::InputKnownNotApplied => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::UnexpectedMutationReply)
            }
        }
    }

    fn resize(&mut self, size: PtySize) -> Result<(), GuardianProxyError> {
        validate_pty_size(size)?;
        self.mutate_generic(GenericMutation::Resize(size))
    }

    fn terminate(&mut self) -> Result<(), GuardianProxyError> {
        self.mutate_generic(GenericMutation::Terminate)
    }

    fn close(&mut self, identity: GuardianPaneLeaseIdentity) -> Result<(), GuardianProxyError> {
        self.ensure_identity(identity)?;
        self.mutate_generic(GenericMutation::Close)
    }

    fn retire(&mut self, identity: GuardianPaneLeaseIdentity) -> Result<(), GuardianProxyError> {
        self.ensure_identity(identity)?;
        self.mutate_generic(GenericMutation::Retire)
    }
}

fn validate_pty_size(size: PtySize) -> Result<(), GuardianProxyError> {
    if size.rows == 0 || size.cols == 0 {
        Err(GuardianProxyError::InvalidConfiguration(
            "PTY rows and columns must be nonzero",
        ))
    } else {
        Ok(())
    }
}

/// Wipe-on-drop buffer with a hard pre-allocation ceiling.
///
/// Replay delivery APIs consume plaintext into an `io::Write`; this sink makes
/// the bound effective before every allocation and never materializes a second
/// raw checkpoint/output copy.
struct BoundedReplayBuffer {
    bytes: Zeroizing<Vec<u8>>,
    maximum: usize,
}

impl BoundedReplayBuffer {
    fn new(maximum: usize) -> Result<Self, GuardianProxyError> {
        if maximum == 0 {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian replay buffer maximum must be nonzero",
            ));
        }
        Ok(Self {
            bytes: Zeroizing::new(Vec::new()),
            maximum,
        })
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    fn copy_out(&mut self, offset: usize, target: &mut [u8]) -> Result<usize, GuardianProxyError> {
        if offset > self.bytes.len() {
            return Err(GuardianProxyError::ReplayInvariant(
                "guardian replay reader offset exceeded its plaintext buffer",
            ));
        }
        let count = target.len().min(self.bytes.len() - offset);
        let end = offset
            .checked_add(count)
            .ok_or(GuardianProxyError::ReplayCapacity)?;
        target[..count].copy_from_slice(&self.bytes[offset..end]);
        self.bytes[offset..end].zeroize();
        Ok(count)
    }

    fn zeroize_and_clear(&mut self) {
        self.bytes.as_mut_slice().zeroize();
        self.bytes.clear();
    }
}

impl Write for BoundedReplayBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("guardian replay buffer length overflow"))?;
        if next > self.maximum {
            return Err(io::Error::other(
                "guardian replay buffer exceeded its configured ceiling",
            ));
        }
        self.bytes
            .try_reserve_exact(bytes.len())
            .map_err(|_| io::Error::other("guardian replay buffer allocation failed"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct GuardianReplayAckPlan {
    snapshot_id: Uuid,
    snapshot_digest: [u8; 32],
    page_index: u32,
    page_digest: [u8; 32],
    next_cursor: Option<GuardianReplayCursorV1>,
    through_sequence: u64,
    through_record_digest: [u8; 32],
    release_if_complete: bool,
    request_id: Uuid,
}

impl GuardianReplayAckPlan {
    fn ack(&self) -> Result<GuardianReplayAckV1, GuardianProxyError> {
        GuardianReplayAckV1::new(
            self.snapshot_id,
            self.snapshot_digest,
            self.page_index,
            self.page_digest,
            self.next_cursor.map(GuardianReplayCursorV1::digest),
            self.through_sequence,
            self.through_record_digest,
            self.release_if_complete,
        )
        .map_err(GuardianProxyError::ReplayProtocol)
    }
}

fn replay_error_is_retryable_io(error: &GuardianProxyError) -> bool {
    matches!(
        error,
        GuardianProxyError::Client(GuardianClientError::Io(_) | GuardianClientError::Setup(_))
    )
}

fn replay_page_with_exact_retry(
    transport: &mut dyn GuardianReplayTransport,
    request_id: Uuid,
    request: GuardianReplayRequestV1,
) -> Result<GuardianReplayPageDelivery, GuardianProxyError> {
    for attempt in 0..GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS {
        match transport.replay(request_id, request) {
            Err(error)
                if replay_error_is_retryable_io(&error)
                    && attempt + 1 < GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS =>
            {
                metrics::counter!(
                    "mux.guardian_proxy.replay_exact_retry_total",
                    "operation" => "replay",
                )
                .increment(1);
            }
            result => return result,
        }
    }
    Err(GuardianProxyError::ReplayInvariant(
        "bounded replay retry loop exhausted without a result",
    ))
}

fn replay_ack_with_exact_retry(
    transport: &mut dyn GuardianReplayTransport,
    plan: &GuardianReplayAckPlan,
) -> Result<(), GuardianProxyError> {
    let ack = plan.ack()?;
    for attempt in 0..GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS {
        match transport.replay_ack(plan.request_id, ack) {
            Ok(receipt) if receipt == GuardianReplayAckReceiptV1::from_ack(ack) => return Ok(()),
            Ok(_) => {
                return Err(GuardianProxyError::ReplayInvariant(
                    "guardian replay acknowledgement receipt did not match its exact request",
                ));
            }
            Err(error)
                if replay_error_is_retryable_io(&error)
                    && attempt + 1 < GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS =>
            {
                metrics::counter!(
                    "mux.guardian_proxy.replay_exact_retry_total",
                    "operation" => "replay_ack",
                )
                .increment(1);
            }
            Err(error) => return Err(error),
        }
    }
    Err(GuardianProxyError::ReplayInvariant(
        "bounded replay acknowledgement retry loop exhausted without a result",
    ))
}

fn validate_replay_page_identity(
    page: &GuardianReplayPageDelivery,
    identity: GuardianPaneLeaseIdentity,
) -> Result<(), GuardianProxyError> {
    if page.header().pane_id() != identity.pane_id()
        || page.header().generation() != identity.generation()
    {
        Err(GuardianProxyError::LeaseIdentityMismatch)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardianReplayBoundary {
    next_sequence: u64,
    previous_record_digest: [u8; 32],
    cumulative_plaintext_bytes: u64,
}

impl GuardianReplayBoundary {
    fn from_descriptor(
        descriptor: GuardianCheckpointDescriptorV1,
    ) -> Result<Self, GuardianProxyError> {
        let GuardianCheckpointOutputBoundaryV1::Record {
            sequence,
            record_digest,
            cumulative_plaintext_bytes,
            ..
        } = descriptor.output_boundary()
        else {
            return Err(GuardianProxyError::ReplayInvariant(
                "a claimed pane replay selected a genesis checkpoint",
            ));
        };
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(GuardianProxyError::ReplayCapacity)?;
        Ok(Self {
            next_sequence,
            previous_record_digest: record_digest,
            cumulative_plaintext_bytes,
        })
    }

    fn through_sequence(self) -> Result<u64, GuardianProxyError> {
        self.next_sequence
            .checked_sub(1)
            .ok_or(GuardianProxyError::ReplayInvariant(
                "guardian replay boundary has no predecessor sequence",
            ))
    }
}

struct InertReplayWriter<'a> {
    terminal: &'a mut InertTerminal,
    failure: Option<InertTerminalError>,
}

impl Write for InertReplayWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failure.is_some() {
            return Err(io::Error::other(
                "guardian inert replay writer is already poisoned",
            ));
        }
        match self.terminal.replay_bytes(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.failure = Some(error);
                Err(io::Error::other("guardian inert terminal rejected replay"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct VerifiedGuardianReplayRestore {
    inert_terminal: InertTerminal,
    checkpoint_id: GuardianCheckpointIdentityDigest,
    boundary: GuardianReplayBoundary,
}

impl fmt::Debug for VerifiedGuardianReplayRestore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedGuardianReplayRestore")
            .field("checkpoint_id", &"[REDACTED]")
            .field("boundary", &self.boundary)
            .field("terminal", &self.inert_terminal)
            .finish()
    }
}

fn validate_checkpoint_descriptor_for_proxy(
    descriptor: GuardianCheckpointDescriptorV1,
    identity: GuardianPaneLeaseIdentity,
    expected_size: PtySize,
    limits: TerminalCheckpointLimits,
) -> Result<(), GuardianProxyError> {
    if descriptor.durable_pane_id() != Some(identity.pane_id()) {
        return Err(GuardianProxyError::LeaseIdentityMismatch);
    }
    if descriptor.capture_generation() > identity.generation() {
        return Err(GuardianProxyError::ReplayInvariant(
            "checkpoint capture generation is newer than the claimed lease fence",
        ));
    }
    if descriptor.rows() != u32::from(expected_size.rows)
        || descriptor.cols() != u32::from(expected_size.cols)
    {
        return Err(GuardianProxyError::ReplayInvariant(
            "checkpoint geometry does not match the claimed topology manifest",
        ));
    }
    if usize::try_from(descriptor.total_bytes())
        .ok()
        .is_none_or(|bytes| bytes == 0 || bytes > limits.max_encoded_bytes)
    {
        return Err(GuardianProxyError::ReplayCapacity);
    }
    GuardianReplayBoundary::from_descriptor(descriptor)?;
    Ok(())
}

fn restore_inert_checkpoint(
    descriptor: GuardianCheckpointDescriptorV1,
    checkpoint: &BoundedReplayBuffer,
    expected_size: PtySize,
    config: Arc<dyn TerminalConfiguration>,
    limits: TerminalCheckpointLimits,
) -> Result<InertTerminal, GuardianProxyError> {
    if u64::try_from(checkpoint.len()) != Ok(descriptor.total_bytes()) {
        return Err(GuardianProxyError::ReplayInvariant(
            "checkpoint replay did not assemble its exact declared length",
        ));
    }
    descriptor
        .validate_canonical_payload(checkpoint.as_slice())
        .map_err(GuardianProxyError::ReplayProtocol)?;
    let validated = TerminalCheckpointV2::decode_canonical_json(checkpoint.as_slice(), limits)
        .map_err(GuardianProxyError::TerminalCheckpoint)?;
    if validated.rows() != descriptor.rows() || validated.cols() != descriptor.cols() {
        return Err(GuardianProxyError::ReplayInvariant(
            "decoded checkpoint geometry differs from its authenticated descriptor",
        ));
    }
    if validated.pixel_width() != u64::from(expected_size.pixel_width)
        || validated.pixel_height() != u64::from(expected_size.pixel_height)
    {
        return Err(GuardianProxyError::ReplayInvariant(
            "checkpoint pixel geometry does not match the claimed topology manifest",
        ));
    }
    validated
        .restore_inert(config)
        .map_err(GuardianProxyError::TerminalCheckpoint)
}

fn replay_record_into_inert_terminal(
    terminal: &mut InertTerminal,
    record: mux::guardian_protocol::GuardianReplayRecordDelivery,
    maximum_record_bytes: u32,
) -> Result<mux::guardian_protocol::GuardianReplayRecordMetadataV1, GuardianProxyError> {
    let expected = record.metadata();
    let mut writer = InertReplayWriter {
        terminal,
        failure: None,
    };
    let delivery = record.write_all_bounded(&mut writer, maximum_record_bytes);
    if let Some(error) = writer.failure.take() {
        return Err(GuardianProxyError::TerminalReplay(error));
    }
    let observed = delivery.map_err(GuardianProxyError::ReplayDelivery)?;
    if observed != expected {
        return Err(GuardianProxyError::ReplayInvariant(
            "consuming replay record returned different authenticated metadata",
        ));
    }
    Ok(observed)
}

fn replay_page_ack_plan(
    snapshot_id: Uuid,
    snapshot_digest: [u8; 32],
    page_index: u32,
    page_digest: [u8; 32],
    next_cursor: Option<GuardianReplayCursorV1>,
    terminal: bool,
    through_sequence: u64,
    through_record_digest: [u8; 32],
) -> GuardianReplayAckPlan {
    GuardianReplayAckPlan {
        snapshot_id,
        snapshot_digest,
        page_index,
        page_digest,
        next_cursor,
        through_sequence,
        through_record_digest,
        release_if_complete: terminal,
        request_id: Uuid::new_v4(),
    }
}

fn consume_one_guardian_replay_snapshot(
    transport: &mut dyn GuardianReplayTransport,
    identity: GuardianPaneLeaseIdentity,
    expected_size: PtySize,
    config: Arc<dyn TerminalConfiguration>,
    limits: TerminalCheckpointLimits,
) -> Result<VerifiedGuardianReplayRestore, GuardianProxyError> {
    let maximum_record_bytes = u32::try_from(limits.max_replay_record_bytes)
        .unwrap_or(u32::MAX)
        .min(GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES);
    if maximum_record_bytes == 0 || limits.max_replay_records == 0 {
        return Err(GuardianProxyError::InvalidConfiguration(
            "guardian terminal replay limits must be nonzero",
        ));
    }
    let maximum_page_records = u16::try_from(limits.max_replay_records)
        .unwrap_or(u16::MAX)
        .min(GUARDIAN_MAX_REPLAY_RECORDS);
    let mut request = GuardianReplayRequestV1::Open {
        selector: GuardianReplaySelectorV1::LatestCompatible,
        max_plaintext_bytes: maximum_record_bytes,
        max_records: maximum_page_records,
        wait_millis: 0,
    };
    let mut checkpoint = BoundedReplayBuffer::new(limits.max_encoded_bytes)?;
    let mut descriptor = None;
    let mut inert_terminal = None;
    let mut boundary = None;

    for _ in 0..GUARDIAN_RESTORE_MAX_PAGES {
        let page = replay_page_with_exact_retry(transport, Uuid::new_v4(), request)?;
        validate_replay_page_identity(&page, identity)?;
        let snapshot_id = page.header().snapshot_id();
        let snapshot_digest = page.header().snapshot_digest();
        let page_index = page.header().page_index();
        let page_digest = page.header().declassify_page_digest_for_ack();
        let next_cursor = page.header().next_cursor();
        let terminal_page = page.is_terminal();

        match page.into_body() {
            GuardianReplayPageBodyDelivery::CheckpointChunk(chunk) => {
                if inert_terminal.is_some() || boundary.is_some() {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "checkpoint bytes arrived after suffix replay began",
                    ));
                }
                let observed_descriptor = chunk.descriptor();
                validate_checkpoint_descriptor_for_proxy(
                    observed_descriptor,
                    identity,
                    expected_size,
                    limits,
                )?;
                if descriptor.is_some_and(|expected| expected != observed_descriptor) {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "checkpoint descriptor changed within one replay snapshot",
                    ));
                }
                descriptor = Some(observed_descriptor);
                let expected_offset = u64::try_from(checkpoint.len())
                    .map_err(|_| GuardianProxyError::ReplayCapacity)?;
                if chunk.offset() != expected_offset {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "checkpoint chunks were not delivered contiguously",
                    ));
                }
                let (_, observed_offset, observed_bytes) = chunk
                    .write_all_bounded(&mut checkpoint, GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES)
                    .map_err(GuardianProxyError::ReplayDelivery)?;
                if observed_offset != expected_offset || observed_bytes == 0 {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "checkpoint chunk delivery changed its authenticated position",
                    ));
                }
                let base = GuardianReplayBoundary::from_descriptor(observed_descriptor)?;
                let through_sequence = base.through_sequence()?;
                let ack = replay_page_ack_plan(
                    snapshot_id,
                    snapshot_digest,
                    page_index,
                    page_digest,
                    next_cursor,
                    terminal_page,
                    through_sequence,
                    base.previous_record_digest,
                );
                replay_ack_with_exact_retry(transport, &ack)?;

                if u64::try_from(checkpoint.len()) == Ok(observed_descriptor.total_bytes()) {
                    let restored = restore_inert_checkpoint(
                        observed_descriptor,
                        &checkpoint,
                        expected_size,
                        Arc::clone(&config),
                        limits,
                    )?;
                    checkpoint.zeroize_and_clear();
                    inert_terminal = Some(restored);
                    boundary = Some(base);
                }
            }
            GuardianReplayPageBodyDelivery::OutputRecords(records) => {
                let restored =
                    inert_terminal
                        .as_mut()
                        .ok_or(GuardianProxyError::ReplayInvariant(
                            "output records arrived before a complete checkpoint",
                        ))?;
                let mut current = boundary.ok_or(GuardianProxyError::ReplayInvariant(
                    "output records arrived without a checkpoint boundary",
                ))?;
                if records.first_sequence() != current.next_sequence
                    || records.previous_record_digest() != current.previous_record_digest
                {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "output page does not continue the exact restored boundary",
                    ));
                }
                for record in records.into_records() {
                    let metadata = record.metadata();
                    if metadata.sequence() != current.next_sequence
                        || metadata.cumulative_plaintext_bytes()
                            != current
                                .cumulative_plaintext_bytes
                                .checked_add(u64::from(metadata.payload_bytes()))
                                .ok_or(GuardianProxyError::ReplayCapacity)?
                    {
                        return Err(GuardianProxyError::ReplayInvariant(
                            "output record does not extend cumulative replay authority",
                        ));
                    }
                    let observed =
                        replay_record_into_inert_terminal(restored, record, maximum_record_bytes)?;
                    current = GuardianReplayBoundary {
                        next_sequence: observed
                            .sequence()
                            .checked_add(1)
                            .ok_or(GuardianProxyError::ReplayCapacity)?,
                        previous_record_digest: observed.record_digest(),
                        cumulative_plaintext_bytes: observed.cumulative_plaintext_bytes(),
                    };
                }
                let through_sequence = current.through_sequence()?;
                let ack = replay_page_ack_plan(
                    snapshot_id,
                    snapshot_digest,
                    page_index,
                    page_digest,
                    next_cursor,
                    terminal_page,
                    through_sequence,
                    current.previous_record_digest,
                );
                replay_ack_with_exact_retry(transport, &ack)?;
                boundary = Some(current);
            }
            GuardianReplayPageBodyDelivery::Complete {
                checkpoint_id,
                through_sequence,
                terminal_record_digest,
                cumulative_plaintext_bytes,
            } => {
                let expected_descriptor = descriptor.ok_or(GuardianProxyError::ReplayInvariant(
                    "replay completed without a checkpoint descriptor",
                ))?;
                let restored =
                    inert_terminal
                        .as_ref()
                        .ok_or(GuardianProxyError::ReplayInvariant(
                            "replay completed without an inert terminal",
                        ))?;
                let current = boundary.ok_or(GuardianProxyError::ReplayInvariant(
                    "replay completed without an output boundary",
                ))?;
                if checkpoint_id != expected_descriptor.checkpoint_id()
                    || through_sequence != current.through_sequence()?
                    || terminal_record_digest != current.previous_record_digest
                    || cumulative_plaintext_bytes != current.cumulative_plaintext_bytes
                    || next_cursor.is_some()
                    || !terminal_page
                {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "terminal replay witness does not match the consumed checkpoint and suffix",
                    ));
                }
                restored
                    .checkpoint()
                    .map_err(GuardianProxyError::TerminalReplay)?;
                let ack = replay_page_ack_plan(
                    snapshot_id,
                    snapshot_digest,
                    page_index,
                    page_digest,
                    next_cursor,
                    terminal_page,
                    through_sequence,
                    terminal_record_digest,
                );
                replay_ack_with_exact_retry(transport, &ack)?;
                return Ok(VerifiedGuardianReplayRestore {
                    inert_terminal: inert_terminal.ok_or(GuardianProxyError::ReplayInvariant(
                        "verified terminal disappeared before activation",
                    ))?,
                    checkpoint_id,
                    boundary: current,
                });
            }
            GuardianReplayPageBodyDelivery::Gap { .. } => {
                let _ = replay_ack_with_exact_retry(
                    transport,
                    &replay_page_ack_plan(
                        snapshot_id,
                        snapshot_digest,
                        page_index,
                        page_digest,
                        next_cursor,
                        terminal_page,
                        0,
                        [0; 32],
                    ),
                );
                return Err(GuardianProxyError::ReplayGap);
            }
            GuardianReplayPageBodyDelivery::Compacted { .. } => {
                let _ = replay_ack_with_exact_retry(
                    transport,
                    &replay_page_ack_plan(
                        snapshot_id,
                        snapshot_digest,
                        page_index,
                        page_digest,
                        next_cursor,
                        terminal_page,
                        0,
                        [0; 32],
                    ),
                );
                return Err(GuardianProxyError::ReplayCompacted);
            }
            GuardianReplayPageBodyDelivery::SnapshotExpired {
                snapshot_id: expired,
            } if expired == snapshot_id => {
                return Err(GuardianProxyError::ReplaySnapshotExpired);
            }
            GuardianReplayPageBodyDelivery::SnapshotExpired { .. } => {
                return Err(GuardianProxyError::ReplayInvariant(
                    "snapshot-expired body named a different replay snapshot",
                ));
            }
        }

        let cursor = next_cursor.ok_or(GuardianProxyError::ReplayInvariant(
            "nonterminal replay page omitted its continuation cursor",
        ))?;
        request = GuardianReplayRequestV1::Continue { cursor };
    }
    Err(GuardianProxyError::ReplayCapacity)
}

fn consume_guardian_replay_for_restore(
    transport: &mut dyn GuardianReplayTransport,
    identity: GuardianPaneLeaseIdentity,
    expected_size: PtySize,
    config: Arc<dyn TerminalConfiguration>,
    limits: TerminalCheckpointLimits,
) -> Result<VerifiedGuardianReplayRestore, GuardianProxyError> {
    for attempt in 0..GUARDIAN_RESTORE_REOPEN_ATTEMPTS {
        match consume_one_guardian_replay_snapshot(
            transport,
            identity,
            expected_size,
            Arc::clone(&config),
            limits,
        ) {
            Err(GuardianProxyError::ReplaySnapshotExpired)
                if attempt + 1 < GUARDIAN_RESTORE_REOPEN_ATTEMPTS =>
            {
                metrics::counter!(
                    "mux.guardian_proxy.replay_snapshot_reopen_total",
                    "phase" => "restore",
                )
                .increment(1);
            }
            result => return result,
        }
    }
    Err(GuardianProxyError::ReplaySnapshotExpired)
}

/// Blocking raw-output reader that resumes from the exact terminal witness
/// proven by off-topology restore.
///
/// A page is acknowledged only after every plaintext byte has been returned to
/// the pane reader. If an acknowledgement snapshot expires after delivery, a
/// fresh Resume snapshot starts strictly after that delivered sequence/digest;
/// no byte is guessed, skipped, or replayed twice into the live parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardianReplayDeferredTerminalError {
    Gap,
}

impl GuardianReplayDeferredTerminalError {
    const fn into_proxy_error(self) -> GuardianProxyError {
        match self {
            Self::Gap => GuardianProxyError::ReplayGap,
        }
    }
}

struct GuardianReplayTailReader {
    transport: Box<dyn GuardianReplayTransport>,
    identity: GuardianPaneLeaseIdentity,
    checkpoint_id: GuardianCheckpointIdentityDigest,
    boundary: GuardianReplayBoundary,
    cursor: Option<GuardianReplayCursorV1>,
    pending_replay: Option<(Uuid, GuardianReplayRequestV1)>,
    records: VecDeque<GuardianReplayRecordDelivery>,
    pending_ack: Option<GuardianReplayAckPlan>,
    pending_boundary: Option<GuardianReplayBoundary>,
    pending_terminal_error: Option<GuardianReplayDeferredTerminalError>,
    delivery_failed: bool,
    maximum_record_bytes: u32,
    maximum_page_records: u16,
    idle_poll_interval: Duration,
}

impl fmt::Debug for GuardianReplayTailReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianReplayTailReader")
            .field("identity", &self.identity)
            .field("checkpoint_id", &"[REDACTED]")
            .field("boundary", &self.boundary)
            .field("has_cursor", &self.cursor.is_some())
            .field("has_pending_replay", &self.pending_replay.is_some())
            .field("buffered_records", &self.records.len())
            .field("has_pending_ack", &self.pending_ack.is_some())
            .field(
                "has_pending_terminal_error",
                &self.pending_terminal_error.is_some(),
            )
            .field("delivery_failed", &self.delivery_failed)
            .finish_non_exhaustive()
    }
}

impl GuardianReplayTailReader {
    fn new(
        transport: Box<dyn GuardianReplayTransport>,
        identity: GuardianPaneLeaseIdentity,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary: GuardianReplayBoundary,
        limits: TerminalCheckpointLimits,
    ) -> Result<Self, GuardianProxyError> {
        let maximum_record_bytes = u32::try_from(limits.max_replay_record_bytes)
            .unwrap_or(u32::MAX)
            .min(GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES);
        let maximum_page_records = u16::try_from(limits.max_replay_records)
            .unwrap_or(u16::MAX)
            .min(GUARDIAN_MAX_REPLAY_RECORDS);
        if maximum_record_bytes == 0 || maximum_page_records == 0 {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian tail replay limits must be nonzero",
            ));
        }
        boundary.through_sequence()?;
        Ok(Self {
            transport,
            identity,
            checkpoint_id,
            boundary,
            cursor: None,
            pending_replay: None,
            records: VecDeque::new(),
            pending_ack: None,
            pending_boundary: None,
            pending_terminal_error: None,
            delivery_failed: false,
            maximum_record_bytes,
            maximum_page_records,
            idle_poll_interval: GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL,
        })
    }

    fn request(&self) -> GuardianReplayRequestV1 {
        self.cursor.map_or(
            GuardianReplayRequestV1::Open {
                selector: GuardianReplaySelectorV1::Resume {
                    checkpoint_id: self.checkpoint_id,
                    next_sequence: self.boundary.next_sequence,
                    previous_record_digest: self.boundary.previous_record_digest,
                },
                max_plaintext_bytes: self.maximum_record_bytes,
                max_records: self.maximum_page_records,
                wait_millis: GUARDIAN_MAX_REPLAY_WAIT_MILLIS,
            },
            |cursor| GuardianReplayRequestV1::Continue { cursor },
        )
    }

    fn finish_delivered_page(&mut self) -> Result<(), GuardianProxyError> {
        let Some(plan) = self.pending_ack else {
            return Ok(());
        };
        if !self.records.is_empty() {
            return Err(GuardianProxyError::ReplayInvariant(
                "guardian replay page was acknowledged before record delivery completed",
            ));
        }
        let delivered = self
            .pending_boundary
            .ok_or(GuardianProxyError::ReplayInvariant(
                "guardian replay page has no delivered terminal boundary",
            ))?;
        // Commit the local delivery fence before attempting Ack. If the
        // process-local snapshot expired, Resume must begin after bytes that
        // the parser has already received.
        self.boundary = delivered;
        match replay_ack_with_exact_retry(self.transport.as_mut(), &plan) {
            Ok(()) => {
                self.cursor = plan.next_cursor;
            }
            Err(GuardianProxyError::ReplaySnapshotExpired) => {
                self.cursor = None;
                metrics::counter!(
                    "mux.guardian_proxy.replay_snapshot_reopen_total",
                    "phase" => "tail_ack",
                )
                .increment(1);
            }
            Err(error) => return Err(error),
        }
        self.pending_ack = None;
        self.pending_boundary = None;
        Ok(())
    }

    fn load_next_output_page(&mut self) -> Result<(), GuardianProxyError> {
        if self.pending_ack.is_some()
            || self.pending_terminal_error.is_some()
            || !self.records.is_empty()
        {
            return Err(GuardianProxyError::ReplayInvariant(
                "guardian tail attempted to fetch past unacknowledged plaintext",
            ));
        }
        for _ in 0..GUARDIAN_RESTORE_MAX_PAGES {
            let (request_id, request) = match self.pending_replay {
                Some(pending) => pending,
                None => {
                    let pending = (Uuid::new_v4(), self.request());
                    self.pending_replay = Some(pending);
                    pending
                }
            };
            let replay_wait_budget = guardian_replay_server_wait_budget(request);
            let replay_started = Instant::now();
            let page = replay_page_with_exact_retry(self.transport.as_mut(), request_id, request)?;
            let replay_elapsed = replay_started.elapsed();
            validate_replay_page_identity(&page, self.identity)?;
            let snapshot_id = page.header().snapshot_id();
            let snapshot_digest = page.header().snapshot_digest();
            let page_index = page.header().page_index();
            let page_digest = page.header().declassify_page_digest_for_ack();
            let next_cursor = page.header().next_cursor();
            let terminal_page = page.is_terminal();

            match page.into_body() {
                GuardianReplayPageBodyDelivery::OutputRecords(records) => {
                    if records.first_sequence() != self.boundary.next_sequence
                        || records.previous_record_digest() != self.boundary.previous_record_digest
                        || next_cursor.is_none()
                        || terminal_page
                    {
                        return Err(GuardianProxyError::ReplayInvariant(
                            "live output page does not continue the delivered parser boundary",
                        ));
                    }
                    let mut candidate = self.boundary;
                    let records = records.into_records();
                    let mut candidate_records = VecDeque::new();
                    candidate_records
                        .try_reserve(records.len())
                        .map_err(|_| GuardianProxyError::ReplayCapacity)?;
                    for record in records {
                        let metadata = record.metadata();
                        if metadata.sequence() != candidate.next_sequence
                            || metadata.payload_bytes() > self.maximum_record_bytes
                            || metadata.cumulative_plaintext_bytes()
                                != candidate
                                    .cumulative_plaintext_bytes
                                    .checked_add(u64::from(metadata.payload_bytes()))
                                    .ok_or(GuardianProxyError::ReplayCapacity)?
                        {
                            return Err(GuardianProxyError::ReplayInvariant(
                                "live output record breaks the delivered sequence/digest chain",
                            ));
                        }
                        candidate_records.push_back(record);
                        candidate = GuardianReplayBoundary {
                            next_sequence: metadata
                                .sequence()
                                .checked_add(1)
                                .ok_or(GuardianProxyError::ReplayCapacity)?,
                            previous_record_digest: metadata.record_digest(),
                            cumulative_plaintext_bytes: metadata.cumulative_plaintext_bytes(),
                        };
                    }
                    if candidate_records.is_empty() {
                        return Err(GuardianProxyError::ReplayInvariant(
                            "live output page contained no records",
                        ));
                    }
                    let pending_ack = replay_page_ack_plan(
                        snapshot_id,
                        snapshot_digest,
                        page_index,
                        page_digest,
                        next_cursor,
                        terminal_page,
                        candidate.through_sequence()?,
                        candidate.previous_record_digest,
                    );
                    self.records = candidate_records;
                    self.pending_ack = Some(pending_ack);
                    self.pending_boundary = Some(candidate);
                    self.pending_replay = None;
                    self.idle_poll_interval = GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL;
                    return Ok(());
                }
                GuardianReplayPageBodyDelivery::Complete {
                    checkpoint_id,
                    through_sequence,
                    terminal_record_digest,
                    cumulative_plaintext_bytes,
                } => {
                    if checkpoint_id != self.checkpoint_id
                        || through_sequence != self.boundary.through_sequence()?
                        || terminal_record_digest != self.boundary.previous_record_digest
                        || cumulative_plaintext_bytes != self.boundary.cumulative_plaintext_bytes
                        || next_cursor.is_some()
                        || !terminal_page
                    {
                        return Err(GuardianProxyError::ReplayInvariant(
                            "live replay completion does not match the delivered parser boundary",
                        ));
                    }
                    let completion_ack = replay_page_ack_plan(
                        snapshot_id,
                        snapshot_digest,
                        page_index,
                        page_digest,
                        next_cursor,
                        terminal_page,
                        through_sequence,
                        terminal_record_digest,
                    );
                    // Persist even a zero-plaintext completion Ack in the
                    // reader state before the first transport attempt. If its
                    // reply is lost beyond the bounded inner retry, the next
                    // `read` must retry this exact request ID rather than issue
                    // a fresh Replay request past an unacknowledged page.
                    self.pending_ack = Some(completion_ack);
                    self.pending_boundary = Some(self.boundary);
                    self.pending_replay = None;
                    self.finish_delivered_page()?;
                    // New guardians hold Resume until durable output or the
                    // authenticated wait deadline. Older guardians return an
                    // empty page immediately. Apply only the portion of the
                    // exponential client fallback that the bounded server
                    // wait did not already consume, so rolling upgrades are
                    // efficient without double-sleeping new peers.
                    let idle_delay = self.idle_poll_interval;
                    self.idle_poll_interval = next_guardian_idle_poll_interval(idle_delay);
                    let remaining_idle_delay = guardian_replay_remaining_idle_delay(
                        idle_delay,
                        replay_wait_budget,
                        replay_elapsed,
                    );
                    if !remaining_idle_delay.is_zero() {
                        thread::sleep(remaining_idle_delay);
                    }
                }
                GuardianReplayPageBodyDelivery::SnapshotExpired {
                    snapshot_id: expired,
                } if expired == snapshot_id => {
                    self.pending_replay = None;
                    self.cursor = None;
                    metrics::counter!(
                        "mux.guardian_proxy.replay_snapshot_reopen_total",
                        "phase" => "tail_page",
                    )
                    .increment(1);
                }
                GuardianReplayPageBodyDelivery::SnapshotExpired { .. } => {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "tail snapshot-expired body named a different replay snapshot",
                    ));
                }
                GuardianReplayPageBodyDelivery::Gap {
                    verified_through_sequence,
                    reason,
                    ..
                } => {
                    self.pending_replay = None;
                    if reason == GuardianReplayGapReasonV1::NoRecoveryBase {
                        if verified_through_sequence != 0 {
                            return Err(GuardianProxyError::ReplayInvariant(
                                "no-recovery-base gap carried a nonzero replay witness",
                            ));
                        }
                        // The current store emits NoRecoveryBase with the
                        // canonical zero witness. Release that terminal
                        // snapshot before surfacing the data-loss fence. If an
                        // Ack reply is lost beyond the bounded inner retry,
                        // retain both the exact Ack ID and the Gap disposition
                        // across the next caller-visible `read`.
                        self.pending_ack = Some(replay_page_ack_plan(
                            snapshot_id,
                            snapshot_digest,
                            page_index,
                            page_digest,
                            next_cursor,
                            terminal_page,
                            0,
                            [0; 32],
                        ));
                        self.pending_boundary = Some(self.boundary);
                        self.pending_terminal_error =
                            Some(GuardianReplayDeferredTerminalError::Gap);
                        self.finish_delivered_page()?;
                        self.pending_terminal_error = None;
                    }
                    return Err(GuardianProxyError::ReplayGap);
                }
                GuardianReplayPageBodyDelivery::Compacted { .. } => {
                    self.pending_replay = None;
                    return Err(GuardianProxyError::ReplayCompacted);
                }
                GuardianReplayPageBodyDelivery::CheckpointChunk(_) => {
                    return Err(GuardianProxyError::ReplayInvariant(
                        "resume tail unexpectedly returned checkpoint plaintext",
                    ));
                }
            }
        }
        Err(GuardianProxyError::ReplayCapacity)
    }
}

fn next_guardian_idle_poll_interval(current: Duration) -> Duration {
    current
        .saturating_mul(2)
        .min(GUARDIAN_REPLAY_IDLE_POLL_MAX_INTERVAL)
}

fn guardian_replay_server_wait_budget(request: GuardianReplayRequestV1) -> Duration {
    match request {
        GuardianReplayRequestV1::Open {
            selector: GuardianReplaySelectorV1::Resume { .. },
            wait_millis,
            ..
        } => Duration::from_millis(u64::from(wait_millis)),
        GuardianReplayRequestV1::Open { .. } | GuardianReplayRequestV1::Continue { .. } => {
            Duration::ZERO
        }
    }
}

fn guardian_replay_remaining_idle_delay(
    idle_delay: Duration,
    server_wait_budget: Duration,
    replay_elapsed: Duration,
) -> Duration {
    idle_delay.saturating_sub(replay_elapsed.min(server_wait_budget))
}

impl GuardianLiveOutputReader for GuardianReplayTailReader {
    fn deliver_next_record(
        &mut self,
        deliver: &mut dyn FnMut(
            mux::guardian_output_journal::GuardianOutputSegmentIdentity,
            mux::guardian_output_journal::GuardianOutputAppendReceipt,
            Arc<[u8]>,
        ) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.delivery_failed {
            return Err(io::Error::from(GuardianProxyError::ReplayInvariant(
                "guardian replay reader is terminal after a failed record delivery",
            )));
        }
        loop {
            if let Some(record) = self.records.pop_front() {
                let delivery = (|| {
                    let metadata = record.metadata();
                    let expected_cumulative = self
                        .boundary
                        .cumulative_plaintext_bytes
                        .checked_add(u64::from(metadata.payload_bytes()))
                        .ok_or_else(|| io::Error::from(GuardianProxyError::ReplayCapacity))?;
                    if metadata.sequence() != self.boundary.next_sequence
                        || metadata.cumulative_plaintext_bytes() != expected_cumulative
                    {
                        return Err(io::Error::from(GuardianProxyError::ReplayInvariant(
                            "live output record no longer matches the delivered parser boundary",
                        )));
                    }
                    let candidate = GuardianReplayBoundary {
                        next_sequence: metadata
                            .sequence()
                            .checked_add(1)
                            .ok_or_else(|| io::Error::from(GuardianProxyError::ReplayCapacity))?,
                        previous_record_digest: metadata.record_digest(),
                        cumulative_plaintext_bytes: metadata.cumulative_plaintext_bytes(),
                    };
                    let (segment, output, payload) = record
                        .into_live_output(self.identity.pane_id())
                        .map_err(GuardianProxyError::ReplayDelivery)
                        .map_err(io::Error::from)?;
                    deliver(segment, output, payload)?;
                    Ok(candidate)
                })();
                let candidate = match delivery {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        self.delivery_failed = true;
                        return Err(error);
                    }
                };
                self.boundary = candidate;
                if self.records.is_empty() {
                    if self.pending_boundary != Some(candidate) {
                        self.delivery_failed = true;
                        return Err(io::Error::from(GuardianProxyError::ReplayInvariant(
                            "delivered replay records omitted their terminal page boundary",
                        )));
                    }
                    self.finish_delivered_page().map_err(io::Error::from)?;
                }
                return Ok(());
            }
            self.finish_delivered_page().map_err(io::Error::from)?;
            if let Some(error) = self.pending_terminal_error.take() {
                return Err(io::Error::from(error.into_proxy_error()));
            }
            self.load_next_output_page().map_err(io::Error::from)?;
        }
    }
}

enum GuardianReplayReaderState {
    Staged,
    Ready(Option<Box<dyn Read + Send>>),
    Taken,
}

impl fmt::Debug for GuardianReplayReaderState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Staged => formatter.write_str("Staged"),
            Self::Ready(Some(_)) => formatter.write_str("Ready(Some(<reader>))"),
            Self::Ready(None) => formatter.write_str("Ready(None)"),
            Self::Taken => formatter.write_str("Taken"),
        }
    }
}

#[derive(Debug)]
struct GuardianReplayReaderSlot {
    state: Mutex<GuardianReplayReaderState>,
}

impl GuardianReplayReaderSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(GuardianReplayReaderState::Staged),
        }
    }

    fn take_reader(&self) -> Result<Box<dyn Read + Send>, GuardianProxyError> {
        let mut state = self.state.lock();
        let prior = std::mem::replace(&mut *state, GuardianReplayReaderState::Taken);
        match prior {
            GuardianReplayReaderState::Ready(Some(reader)) => Ok(reader),
            GuardianReplayReaderState::Ready(None)
            | GuardianReplayReaderState::Staged
            | GuardianReplayReaderState::Taken => {
                *state = prior;
                Err(GuardianProxyError::InvalidConfiguration(
                    "guardian replay reader is unavailable before exact restore activation or after its single take",
                ))
            }
        }
    }

    #[cfg(test)]
    fn install_after_restore(
        &self,
        reader: Box<dyn Read + Send>,
    ) -> Result<(), GuardianProxyError> {
        let mut state = self.state.lock();
        if !matches!(*state, GuardianReplayReaderState::Staged) {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian replay reader activation was already attempted",
            ));
        }
        *state = GuardianReplayReaderState::Ready(Some(reader));
        Ok(())
    }

    #[cfg(test)]
    fn install_after_test_restore(
        &self,
        reader: Box<dyn Read + Send>,
    ) -> Result<(), GuardianProxyError> {
        self.install_after_restore(reader)
    }
}

/// A connected, already-claimed guardian lease that is still off topology.
///
/// No portable-pty object and no reader activation method are exposed from this
/// type. The consuming restore transaction must bind terminal restoration to
/// its authenticated final sequence/digest and retain a record-aware live
/// reader before any caller can construct a pane.
pub struct GuardianProxyStaging {
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
    reader_slot: Arc<GuardianReplayReaderSlot>,
    replay_transport: Option<Box<dyn GuardianReplayTransport>>,
    lease_rollback: GuardianClaimedLeaseRollback,
}

impl fmt::Debug for GuardianProxyStaging {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (identity, next_sequence) = {
            let actor = self.actor.lock();
            (actor.identity(), actor.next_sequence())
        };
        formatter
            .debug_struct("GuardianProxyStaging")
            .field("identity", &identity)
            .field("next_sequence", &next_sequence)
            .field("census_guardian", &self.census.guardian_incarnation())
            .field("census_mux", &self.census.mux_incarnation())
            .field("reader_state", &self.reader_slot.state.lock())
            .field("has_replay_transport", &self.replay_transport.is_some())
            .field("lease_rollback_armed", &self.lease_rollback.armed)
            .finish_non_exhaustive()
    }
}

/// Drop guard for an already-claimed lease that has not reached `LocalPane`.
///
/// The mutation actor owns the idempotency record, so a lost retirement reply
/// can be retried here with the exact same sequence/request/effect tuple. Once
/// a `LocalPane` exists, its guardian ownership state becomes the sole lifetime
/// authority and this guard is explicitly disarmed.
struct GuardianClaimedLeaseRollback {
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
    identity: GuardianPaneLeaseIdentity,
    armed: bool,
}

impl GuardianClaimedLeaseRollback {
    fn new(
        actor: SharedGuardianPaneLeaseActor,
        census: Arc<GuardianCensusCoordinator>,
        identity: GuardianPaneLeaseIdentity,
    ) -> Self {
        Self {
            actor,
            census,
            identity,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn retire_unpublished_lease(&mut self) {
        if !self.armed {
            return;
        }
        self.census.invalidate();
        let first_result = self.actor.lock().retire(self.identity);
        match first_result {
            Ok(()) => self.armed = false,
            Err(error) if guardian_cleanup_retry_is_safe(&error) => {
                log::warn!(
                    "guardian lease retirement reply was lost before topology publication; retrying the exact pending mutation: {error}"
                );
                match self.actor.lock().retire(self.identity) {
                    Ok(()) => self.armed = false,
                    Err(retry_error) => log::error!(
                        "guardian lease retirement after unpublished restore was not confirmed after an exact retry: {retry_error}"
                    ),
                }
            }
            Err(error) => log::error!(
                "guardian lease retirement after unpublished restore was not confirmed: {error}"
            ),
        }
    }
}

impl Drop for GuardianClaimedLeaseRollback {
    fn drop(&mut self) {
        self.retire_unpublished_lease();
    }
}

fn guardian_cleanup_retry_is_safe(error: &GuardianProxyError) -> bool {
    matches!(
        error,
        GuardianProxyError::Client(GuardianClientError::Io(_) | GuardianClientError::Setup(_))
    )
}

impl GuardianProxyStaging {
    /// Connect to the guardian and stage proxy authority for one lease already
    /// proven by Claim or Attach.  This function performs neither operation.
    pub fn connect(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        census: Arc<GuardianCensusCoordinator>,
    ) -> Result<Self, GuardianProxyError> {
        census.ensure_binding(identity)?;
        if next_sequence == 0 {
            return Err(GuardianProxyError::InvalidConfiguration(
                "next mutation sequence must be nonzero",
            ));
        }
        validate_pty_size(size)?;
        let mutation_transport =
            GuardianClientTransport::connect(socket_path, token_path, identity)?;
        let mut staging = Self::with_transports(
            identity,
            next_sequence,
            size,
            Box::new(mutation_transport),
            census,
        )?;
        // Stage the rollback guard before opening the independent replay
        // channel. A replay-channel setup failure must not leak the Claim that
        // the caller completed before entering this constructor.
        let replay_transport =
            GuardianReplayClientTransport::connect(socket_path, token_path, identity)?;
        staging.replay_transport = Some(Box::new(replay_transport));
        Ok(staging)
    }

    fn with_transports(
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        transport: Box<dyn GuardianMutationTransport>,
        census: Arc<GuardianCensusCoordinator>,
    ) -> Result<Self, GuardianProxyError> {
        census.ensure_binding(identity)?;
        let actor = Arc::new(Mutex::new(GuardianPaneLeaseActor::with_transport(
            identity,
            next_sequence,
            size,
            transport,
        )?));
        // Claim/Attach completed before staging construction and may postdate
        // an otherwise fresh shared snapshot. Never publish a new pane
        // against a cache that could still describe the pre-claim fleet.
        census.invalidate();
        let lease_rollback =
            GuardianClaimedLeaseRollback::new(Arc::clone(&actor), Arc::clone(&census), identity);
        Ok(Self {
            actor,
            census,
            reader_slot: Arc::new(GuardianReplayReaderSlot::new()),
            replay_transport: None,
            lease_rollback,
        })
    }

    /// Return the exact immutable lease identity.
    #[must_use]
    pub fn identity(&self) -> GuardianPaneLeaseIdentity {
        self.actor.lock().identity()
    }

    /// Return the shared serialization authority used by every eventual proxy
    /// facet.  Its fields remain private.
    #[must_use]
    pub fn shared_actor(&self) -> SharedGuardianPaneLeaseActor {
        Arc::clone(&self.actor)
    }

    /// Consume the authenticated checkpoint/output replay while this pane is
    /// still absent from mux topology, bind a resumable authenticated-record
    /// reader, and activate the terminal's live guardian writer.
    ///
    /// This method never registers the returned pane. Callers must first build
    /// the desired tab/window topology off to the side, convert the result with
    /// [`ActivatedGuardianProxy::into_local_pane`], and use the mux's atomic
    /// pane-registration path. Any replay gap, compaction race, configuration
    /// drift, or reader/writer activation failure leaves no `LocalPane` to
    /// publish.
    pub fn restore_and_activate(
        mut self,
        config: Arc<dyn TerminalConfiguration>,
        limits: TerminalCheckpointLimits,
    ) -> Result<ActivatedGuardianProxy, GuardianProxyError> {
        let identity = self.identity();
        let expected_size = self.actor.lock().size;
        let mut replay_transport =
            self.replay_transport
                .take()
                .ok_or(GuardianProxyError::InvalidConfiguration(
                    "guardian staging has no replay transport bound to its claimed lease",
                ))?;
        let verified = consume_guardian_replay_for_restore(
            replay_transport.as_mut(),
            identity,
            expected_size,
            config,
            limits,
        )?;
        let tail = GuardianReplayTailReader::new(
            replay_transport,
            identity,
            verified.checkpoint_id,
            verified.boundary,
            limits,
        )?;
        self.activate_verified_restore(verified.inert_terminal, Box::new(tail))
    }

    fn activate_verified_restore(
        self,
        inert_terminal: InertTerminal,
        guardian_live_output_reader: Box<dyn GuardianLiveOutputReader>,
    ) -> Result<ActivatedGuardianProxy, GuardianProxyError> {
        let identity = self.identity();
        let terminal_writer = GuardianProxyWriter {
            actor: Arc::clone(&self.actor),
        };
        let terminal = match inert_terminal.into_live(Box::new(terminal_writer.clone())) {
            Ok(terminal) => terminal,
            Err(failure) => {
                let (error, _inert_terminal) = failure.into_parts();
                log::error!(
                    "guardian terminal activation failed before topology publication: {error}"
                );
                return Err(GuardianProxyError::TerminalActivation);
            }
        };
        let actor = Arc::clone(&self.actor);
        Ok(ActivatedGuardianProxy {
            terminal,
            process: Box::new(GuardianProxyChild {
                actor: Arc::clone(&actor),
                census: Arc::clone(&self.census),
            }),
            pty: Box::new(GuardianProxyMasterPty {
                actor: Arc::clone(&actor),
                reader_slot: Arc::clone(&self.reader_slot),
            }),
            writer: Box::new(GuardianProxyWriter {
                actor: Arc::clone(&actor),
            }),
            lease_control: Arc::new(GuardianProxyLeaseControl {
                actor,
                census: Arc::clone(&self.census),
            }),
            guardian_live_output_reader: Some(guardian_live_output_reader),
            lease_identity: identity,
            lease_rollback: self.lease_rollback,
        })
    }

    #[cfg(test)]
    fn activate_after_inert_restore_for_test(
        self,
        inert_terminal: InertTerminal,
        reader: Box<dyn Read + Send>,
    ) -> TestActivatedGuardianProxy {
        let terminal_writer = GuardianProxyWriter {
            actor: Arc::clone(&self.actor),
        };
        let terminal = inert_terminal
            .into_live(Box::new(terminal_writer.clone()))
            .expect("test inert terminal must accept the guardian writer");
        self.reader_slot
            .install_after_test_restore(reader)
            .expect("test reader slot must still be staged");
        let identity = self.identity();
        let actor = Arc::clone(&self.actor);
        TestActivatedGuardianProxy {
            terminal,
            process: Box::new(GuardianProxyChild {
                actor: Arc::clone(&actor),
                census: Arc::clone(&self.census),
            }),
            pty: Box::new(GuardianProxyMasterPty {
                actor: Arc::clone(&actor),
                reader_slot: Arc::clone(&self.reader_slot),
            }),
            writer: Box::new(GuardianProxyWriter {
                actor: Arc::clone(&actor),
            }),
            lease_control: Arc::new(GuardianProxyLeaseControl {
                actor,
                census: Arc::clone(&self.census),
            }),
            guardian_live_output_reader: None,
            lease_identity: identity,
            lease_rollback: self.lease_rollback,
        }
    }
}

/// Fully restored guardian proxy facets that remain unpublished until the
/// caller deliberately constructs and registers a [`LocalPane`].
pub struct ActivatedGuardianProxy {
    terminal: Terminal,
    process: Box<dyn Child + Send>,
    pty: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    lease_control: Arc<dyn GuardianPaneLeaseControl>,
    guardian_live_output_reader: Option<Box<dyn GuardianLiveOutputReader>>,
    lease_identity: GuardianPaneLeaseIdentity,
    lease_rollback: GuardianClaimedLeaseRollback,
}

impl fmt::Debug for ActivatedGuardianProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActivatedGuardianProxy")
            .field("lease_identity", &self.lease_identity)
            .finish_non_exhaustive()
    }
}

impl ActivatedGuardianProxy {
    /// Construct the guardian-backed LocalPane without publishing it.
    #[must_use]
    pub fn into_local_pane(
        mut self,
        pane_id: PaneId,
        domain_id: DomainId,
        command_description: String,
    ) -> LocalPane {
        let pane = LocalPane::new_guardian_proxy(
            pane_id,
            self.terminal,
            self.process,
            self.pty,
            self.writer,
            domain_id,
            self.lease_identity,
            Arc::clone(&self.lease_control),
            command_description,
            self.guardian_live_output_reader
                .take()
                .expect("verified guardian activation must retain its record-aware reader"),
        );
        // Construction completed, so the LocalPane's guardian ownership is
        // now the sole close/retire authority. If construction unwinds before
        // this point, the still-armed rollback guard releases the lease.
        self.lease_rollback.disarm();
        pane
    }
}

#[cfg(test)]
type TestActivatedGuardianProxy = ActivatedGuardianProxy;

#[derive(Clone)]
/// Opaque guardian-backed portable-pty writer facet.
///
/// Its construction remains module-private until replay restoration can
/// publish all proxy facets atomically.
pub struct GuardianProxyWriter {
    actor: SharedGuardianPaneLeaseActor,
}

impl fmt::Debug for GuardianProxyWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianProxyWriter")
            .field("identity", &self.actor.lock().identity())
            .finish_non_exhaustive()
    }
}

impl Write for GuardianProxyWriter {
    fn write(&mut self, payload: &[u8]) -> io::Result<usize> {
        self.actor
            .lock()
            .write_input(payload)
            .map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A successful Input reply already includes the guardian's durable
        // terminal disposition.  Flush therefore emits no protocol bytes; it
        // only reconciles an earlier ambiguous call, if one exists.
        self.actor.lock().flush_pending().map_err(io::Error::from)
    }
}

/// Opaque guardian-backed portable-pty master facet.
pub struct GuardianProxyMasterPty {
    actor: SharedGuardianPaneLeaseActor,
    reader_slot: Arc<GuardianReplayReaderSlot>,
}

impl fmt::Debug for GuardianProxyMasterPty {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianProxyMasterPty")
            .field("identity", &self.actor.lock().identity())
            .finish_non_exhaustive()
    }
}

impl MasterPty for GuardianProxyMasterPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        self.actor.lock().resize(size).map_err(anyhow::Error::new)
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        Ok(self.actor.lock().size)
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        self.reader_slot.take_reader().map_err(anyhow::Error::new)
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        anyhow::bail!("guardian proxy writer was already split during exact restore activation")
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

#[derive(Clone)]
/// Opaque guardian-backed portable-pty child facet.
pub struct GuardianProxyChild {
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
}

impl fmt::Debug for GuardianProxyChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianProxyChild")
            .field("identity", &self.actor.lock().identity())
            .finish_non_exhaustive()
    }
}

impl ChildKiller for GuardianProxyChild {
    fn kill(&mut self) -> io::Result<()> {
        let result = self.actor.lock().terminate().map_err(io::Error::from);
        self.census.invalidate();
        result
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for GuardianProxyChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let (identity, starting_disposition) = {
            let actor = self.actor.lock();
            match actor.disposition {
                GuardianLeaseDisposition::Attached
                | GuardianLeaseDisposition::TerminalObserved
                | GuardianLeaseDisposition::Closed => {}
                GuardianLeaseDisposition::Retired => {
                    return Err(io::Error::from(GuardianProxyError::LeaseNotAttached));
                }
                GuardianLeaseDisposition::Fenced => {
                    return Err(io::Error::from(GuardianProxyError::LeaseFenced));
                }
                GuardianLeaseDisposition::Quarantined => {
                    return Err(io::Error::from(GuardianProxyError::PaneQuarantined));
                }
                GuardianLeaseDisposition::RestoreRequired => {
                    return Err(io::Error::from(GuardianProxyError::ReplaySnapshotExpired));
                }
            }
            (actor.identity(), actor.disposition)
        };
        if starting_disposition == GuardianLeaseDisposition::Closed {
            // A mutation may learn `PaneTerminal` without flowing through the
            // explicit lease-control close facet. Never let a pre-terminal
            // cached Running row answer that newly closed disposition.
            self.census.invalidate();
        }
        // The potentially paginated census owns only the guardian-scoped
        // coordinator lock. The pane mutation actor is reacquired afterward
        // solely to persist a terminal fence/quarantine classification.
        let observation = self.census.observe_child(identity);
        let mut actor = self.actor.lock();
        if actor.disposition != starting_disposition {
            let failure = match actor.disposition {
                GuardianLeaseDisposition::TerminalObserved
                | GuardianLeaseDisposition::Retired
                | GuardianLeaseDisposition::Closed => GuardianProxyError::LeaseNotAttached,
                GuardianLeaseDisposition::Fenced => GuardianProxyError::LeaseFenced,
                GuardianLeaseDisposition::Quarantined => GuardianProxyError::PaneQuarantined,
                GuardianLeaseDisposition::RestoreRequired => {
                    GuardianProxyError::ReplaySnapshotExpired
                }
                GuardianLeaseDisposition::Attached => {
                    actor.disposition = GuardianLeaseDisposition::Quarantined;
                    GuardianProxyError::UnexpectedMutationReply
                }
            };
            return Err(io::Error::from(failure));
        }
        let observed = match observation {
            Ok(observed) => observed,
            Err(error) => {
                let failure = actor.transport_failure(error);
                return Err(io::Error::from(failure));
            }
        };
        match observed {
            ObservedChildState::Running
                if starting_disposition == GuardianLeaseDisposition::Attached =>
            {
                drop(actor);
                Ok(None)
            }
            ObservedChildState::Running => {
                actor.disposition = GuardianLeaseDisposition::Quarantined;
                Err(io::Error::from(GuardianProxyError::UnexpectedMutationReply))
            }
            ObservedChildState::Exited(status) => {
                actor.disposition = if actor.pending.is_some() {
                    GuardianLeaseDisposition::TerminalObserved
                } else {
                    GuardianLeaseDisposition::Closed
                };
                drop(actor);
                Ok(Some(exit_status(status)))
            }
        }
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            thread::sleep(CHILD_STATUS_POLL_INTERVAL);
        }
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

#[derive(Clone)]
/// Opaque guardian-backed mux lease-control facet.
pub struct GuardianProxyLeaseControl {
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
}

impl GuardianPaneLeaseControl for GuardianProxyLeaseControl {
    fn close(&self, identity: GuardianPaneLeaseIdentity) -> anyhow::Result<()> {
        let result = self
            .actor
            .lock()
            .close(identity)
            .map_err(anyhow::Error::new);
        self.census.invalidate();
        result
    }

    fn retire(&self, identity: GuardianPaneLeaseIdentity) -> anyhow::Result<()> {
        let result = self
            .actor
            .lock()
            .retire(identity)
            .map_err(anyhow::Error::new);
        self.census.invalidate();
        result
    }
}

fn exit_status(status: i32) -> ExitStatus {
    match u32::try_from(status) {
        Ok(code) => ExitStatus::with_exit_code(code),
        Err(_) => {
            let signal = status
                .checked_neg()
                .map_or_else(|| "unknown".to_string(), |signal| signal.to_string());
            ExitStatus::with_signal(&format!("signal {signal}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::Mux;
    use mux::guardian_output_journal::{
        GuardianOutputAppendReceipt, GuardianOutputCipher, GuardianOutputJournal,
        GuardianOutputJournalLimits, GuardianOutputSegmentIdentity,
    };
    use mux::guardian_protocol::{
        GuardianCheckpointChunkDelivery, GuardianReplayOutputRecordsDelivery,
        GuardianReplayPhaseV1, GuardianReplayRecordDelivery, GuardianReplayRecordMetadataV1,
    };
    use mux::pane::{Pane, alloc_pane_id};
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use wezterm_term::color::ColorPalette;
    use wezterm_term::terminalstate::checkpoint::{TerminalCheckpointLimits, TerminalCheckpointV2};
    use wezterm_term::{InertTerminal, Terminal, TerminalConfiguration, TerminalSize};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeDirective {
        Auto,
        Io,
        Query(InputEffectState),
        Reject(GuardianRejectionCode),
        Observe(ObservedChildState),
        ObserveMissingExitStatus,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum FakeCall {
        Input {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            input_bytes: u32,
            payload_sha256: [u8; 32],
        },
        QueryInput {
            request_id: Uuid,
            effect_id: Uuid,
        },
        Resize {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            size: PtySize,
        },
        Terminate {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        },
        Close {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        },
        Retire {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        },
        Census,
    }

    #[derive(Default)]
    struct FakeState {
        directives: VecDeque<FakeDirective>,
        calls: Vec<FakeCall>,
    }

    #[derive(Clone)]
    struct FakeTransport {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Clone)]
    struct FakeCensusTransport {
        state: Arc<Mutex<FakeState>>,
        identity: GuardianPaneLeaseIdentity,
    }

    struct BlockingCensusTransport {
        identity: GuardianPaneLeaseIdentity,
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    struct ScriptedCensusState {
        calls: usize,
        snapshots: VecDeque<Vec<GuardianCensusEntry>>,
    }

    struct ScriptedCensusTransport {
        state: Arc<Mutex<ScriptedCensusState>>,
        entered_once: Option<SyncSender<()>>,
        release_once: Option<Receiver<()>>,
    }

    struct FakeReplayState {
        pages: VecDeque<GuardianReplayPageDelivery>,
        replay_io_failures: usize,
        ack_io_failures: usize,
        requests: Vec<(Uuid, GuardianReplayRequestV1)>,
        acks: Vec<(Uuid, GuardianReplayAckV1)>,
    }

    #[derive(Clone)]
    struct FakeReplayTransport {
        state: Arc<Mutex<FakeReplayState>>,
    }

    impl GuardianReplayTransport for FakeReplayTransport {
        fn replay(
            &mut self,
            request_id: Uuid,
            request: GuardianReplayRequestV1,
        ) -> Result<GuardianReplayPageDelivery, GuardianProxyError> {
            let mut state = self.state.lock();
            state.requests.push((request_id, request));
            if state.replay_io_failures > 0 {
                state.replay_io_failures -= 1;
                return Err(GuardianProxyError::Client(GuardianClientError::Io(
                    io::Error::new(io::ErrorKind::ConnectionReset, "injected lost replay reply"),
                )));
            }
            state
                .pages
                .pop_front()
                .ok_or(GuardianProxyError::ReplayInvariant(
                    "fake replay transport has no remaining page",
                ))
        }

        fn replay_ack(
            &mut self,
            request_id: Uuid,
            ack: GuardianReplayAckV1,
        ) -> Result<GuardianReplayAckReceiptV1, GuardianProxyError> {
            let mut state = self.state.lock();
            state.acks.push((request_id, ack));
            if state.ack_io_failures > 0 {
                state.ack_io_failures -= 1;
                return Err(GuardianProxyError::Client(GuardianClientError::Io(
                    io::Error::new(io::ErrorKind::ConnectionReset, "injected lost ack reply"),
                )));
            }
            Ok(GuardianReplayAckReceiptV1::from_ack(ack))
        }
    }

    struct ChannelGuardianReader {
        receiver: Receiver<(
            GuardianOutputSegmentIdentity,
            GuardianOutputAppendReceipt,
            Arc<[u8]>,
        )>,
    }

    impl GuardianLiveOutputReader for ChannelGuardianReader {
        fn deliver_next_record(
            &mut self,
            deliver: &mut dyn FnMut(
                GuardianOutputSegmentIdentity,
                GuardianOutputAppendReceipt,
                Arc<[u8]>,
            ) -> io::Result<()>,
        ) -> io::Result<()> {
            let (segment, output, payload) = self
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "test guardian reader closed")
                    }
                    std::sync::mpsc::RecvTimeoutError::Timeout => io::Error::new(
                        io::ErrorKind::TimedOut,
                        "test guardian reader timed out",
                    ),
                })?;
            deliver(segment, output, payload)
        }
    }

    struct RecordCheckpointFixture {
        descriptor: GuardianCheckpointDescriptorV1,
        checkpoint: Zeroizing<Vec<u8>>,
        segment: GuardianOutputSegmentIdentity,
        receipt: GuardianOutputAppendReceipt,
    }

    impl FakeTransport {
        fn record(&self, call: FakeCall) -> FakeDirective {
            let mut state = self.state.lock();
            state.calls.push(call);
            state.directives.pop_front().unwrap_or(FakeDirective::Auto)
        }

        fn reply_error(
            directive: FakeDirective,
        ) -> Option<Result<GuardianReply, GuardianMutationTransportError>> {
            match directive {
                FakeDirective::Io => Some(Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected lost reply",
                    )),
                ))),
                FakeDirective::Reject(code) => Some(Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Rejected(code),
                ))),
                FakeDirective::Auto
                | FakeDirective::Query(_)
                | FakeDirective::Observe(_)
                | FakeDirective::ObserveMissingExitStatus => None,
            }
        }
    }

    impl GuardianMutationTransport for FakeTransport {
        fn input(
            &mut self,
            pane_id: Uuid,
            generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            payload: Vec<u8>,
        ) -> Result<GuardianReply, GuardianMutationTransportError> {
            let input_bytes = u32::try_from(payload.len()).expect("bounded fake input length");
            let directive = self.record(FakeCall::Input {
                sequence,
                request_id,
                effect_id,
                input_bytes,
                payload_sha256: Sha256::digest(&payload).into(),
            });
            if let Some(result) = Self::reply_error(directive) {
                return result;
            }
            Ok(GuardianReply::InputReceipt {
                pane_id,
                generation,
                sequence,
                effect_id,
                state: InputEffectState::DurableFull,
            })
        }

        fn query_input_effect(
            &mut self,
            _pane_id: Uuid,
            _generation: u64,
            request_id: Uuid,
            effect_id: Uuid,
            _query: GuardianInputEffectQuery,
        ) -> Result<InputEffectState, GuardianMutationTransportError> {
            let directive = self.record(FakeCall::QueryInput {
                request_id,
                effect_id,
            });
            match directive {
                FakeDirective::Query(state) => Ok(state),
                FakeDirective::Auto => Ok(InputEffectState::DurableFull),
                FakeDirective::Io => Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected lost query reply",
                    )),
                )),
                FakeDirective::Reject(code) => Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Rejected(code),
                )),
                FakeDirective::Observe(_) | FakeDirective::ObserveMissingExitStatus => {
                    panic!("observe directive routed to input query")
                }
            }
        }

        fn resize(
            &mut self,
            pane_id: Uuid,
            generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            size: PtySize,
        ) -> Result<GuardianReply, GuardianMutationTransportError> {
            let directive = self.record(FakeCall::Resize {
                sequence,
                request_id,
                effect_id,
                size,
            });
            if let Some(result) = Self::reply_error(directive) {
                return result;
            }
            Ok(GuardianReply::MutationApplied {
                pane_id,
                generation,
                sequence,
            })
        }

        fn terminate(
            &mut self,
            pane_id: Uuid,
            generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        ) -> Result<GuardianReply, GuardianMutationTransportError> {
            let directive = self.record(FakeCall::Terminate {
                sequence,
                request_id,
                effect_id,
            });
            if let Some(result) = Self::reply_error(directive) {
                return result;
            }
            Ok(GuardianReply::MutationApplied {
                pane_id,
                generation,
                sequence,
            })
        }

        fn close(
            &mut self,
            pane_id: Uuid,
            generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        ) -> Result<GuardianReply, GuardianMutationTransportError> {
            let directive = self.record(FakeCall::Close {
                sequence,
                request_id,
                effect_id,
            });
            if let Some(result) = Self::reply_error(directive) {
                return result;
            }
            Ok(GuardianReply::MutationApplied {
                pane_id,
                generation,
                sequence,
            })
        }

        fn retire(
            &mut self,
            pane_id: Uuid,
            generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
        ) -> Result<GuardianReply, GuardianMutationTransportError> {
            let directive = self.record(FakeCall::Retire {
                sequence,
                request_id,
                effect_id,
            });
            if let Some(result) = Self::reply_error(directive) {
                return result;
            }
            Ok(GuardianReply::LeaseRetired {
                pane_id,
                generation,
            })
        }
    }

    impl GuardianCensusTransport for FakeCensusTransport {
        fn census_snapshot(
            &mut self,
        ) -> Result<Vec<GuardianCensusEntry>, GuardianMutationTransportError> {
            let directive = {
                let mut state = self.state.lock();
                state.calls.push(FakeCall::Census);
                state.directives.pop_front().unwrap_or(FakeDirective::Auto)
            };
            match directive {
                FakeDirective::Observe(state) => {
                    Ok(vec![observed_census_entry(self.identity, state)])
                }
                FakeDirective::Auto => Ok(vec![observed_census_entry(
                    self.identity,
                    ObservedChildState::Running,
                )]),
                FakeDirective::Io => Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected census failure",
                    )),
                )),
                FakeDirective::Reject(code) => Err(GuardianMutationTransportError::Client(
                    GuardianClientError::Rejected(code),
                )),
                FakeDirective::Query(_) => panic!("query directive routed to child observation"),
                FakeDirective::ObserveMissingExitStatus => Ok(vec![GuardianCensusEntry {
                    pane_id: self.identity.pane_id(),
                    status: GuardianCensusPaneStatus::ClosedTerminal,
                    generation: self.identity.generation(),
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    indeterminate_checkpoint_effect: None,
                    exit_status: None,
                    quarantine_reason: None,
                }]),
            }
        }
    }

    impl GuardianCensusTransport for BlockingCensusTransport {
        fn census_snapshot(
            &mut self,
        ) -> Result<Vec<GuardianCensusEntry>, GuardianMutationTransportError> {
            self.entered.send(()).map_err(|_| {
                GuardianMutationTransportError::Client(GuardianClientError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test observation entry receiver disappeared",
                )))
            })?;
            self.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| {
                    GuardianMutationTransportError::Client(GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "test observation release timed out",
                    )))
                })?;
            Ok(vec![observed_census_entry(
                self.identity,
                ObservedChildState::Running,
            )])
        }
    }

    impl GuardianCensusTransport for ScriptedCensusTransport {
        fn census_snapshot(
            &mut self,
        ) -> Result<Vec<GuardianCensusEntry>, GuardianMutationTransportError> {
            let snapshot = {
                let mut state = self.state.lock();
                state.calls = state.calls.saturating_add(1);
                state.snapshots.pop_front().ok_or({
                    GuardianMutationTransportError::Client(GuardianClientError::UnexpectedReply)
                })?
            };
            if let Some(entered) = self.entered_once.take() {
                entered.send(()).map_err(|_| {
                    GuardianMutationTransportError::Client(GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "scripted census entry receiver disappeared",
                    )))
                })?;
            }
            if let Some(release) = self.release_once.take() {
                release.recv_timeout(Duration::from_secs(5)).map_err(|_| {
                    GuardianMutationTransportError::Client(GuardianClientError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "scripted census release timed out",
                    )))
                })?;
            }
            Ok(snapshot)
        }
    }

    #[derive(Debug)]
    struct TestTerminalConfig;

    impl TerminalConfiguration for TestTerminalConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn identity() -> GuardianPaneLeaseIdentity {
        GuardianPaneLeaseIdentity::new(id(1), id(2), id(3), 4)
            .expect("valid guardian lease identity")
    }

    fn identity_for(pane_id: Uuid, generation: u64) -> GuardianPaneLeaseIdentity {
        GuardianPaneLeaseIdentity::new(
            identity().guardian_incarnation(),
            identity().mux_incarnation(),
            pane_id,
            generation,
        )
        .expect("valid guardian lease identity")
    }

    fn observed_census_entry(
        identity: GuardianPaneLeaseIdentity,
        observed: ObservedChildState,
    ) -> GuardianCensusEntry {
        match observed {
            ObservedChildState::Running => GuardianCensusEntry {
                pane_id: identity.pane_id(),
                status: GuardianCensusPaneStatus::LiveClaimed,
                generation: identity.generation(),
                mux_incarnation: Some(identity.mux_incarnation()),
                next_sequence: Some(1),
                pending_input_effect: None,
                indeterminate_checkpoint_effect: None,
                exit_status: None,
                quarantine_reason: None,
            },
            ObservedChildState::Exited(exit_status) => GuardianCensusEntry {
                pane_id: identity.pane_id(),
                status: GuardianCensusPaneStatus::ExitedUnclaimed,
                generation: identity.generation(),
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                indeterminate_checkpoint_effect: None,
                exit_status: Some(exit_status),
                quarantine_reason: None,
            },
        }
    }

    fn size(rows: u16, cols: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: cols.saturating_mul(8),
            pixel_height: rows.saturating_mul(16),
        }
    }

    fn fake_staging(
        directives: impl IntoIterator<Item = FakeDirective>,
        next_sequence: u64,
    ) -> (GuardianProxyStaging, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState {
            directives: directives.into_iter().collect(),
            calls: Vec::new(),
        }));
        let staging = GuardianProxyStaging::with_transports(
            identity(),
            next_sequence,
            size(24, 80),
            Box::new(FakeTransport {
                state: Arc::clone(&state),
            }),
            Arc::new(
                GuardianCensusCoordinator::with_transport(
                    identity().guardian_incarnation(),
                    identity().mux_incarnation(),
                    GUARDIAN_CENSUS_CACHE_MAX_AGE,
                    Box::new(FakeCensusTransport {
                        state: Arc::clone(&state),
                        identity: identity(),
                    }),
                )
                .expect("construct fake census coordinator"),
            ),
        )
        .expect("stage fake guardian proxy");
        (staging, state)
    }

    fn inert_terminal() -> InertTerminal {
        let config = test_terminal_config();
        let terminal = Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::clone(&config),
            "FrankenTerm",
            "guardian-proxy-test",
            Box::new(Vec::<u8>::new()),
        );
        let limits = TerminalCheckpointLimits::default();
        let canonical = terminal
            .capture_recovery_checkpoint(limits)
            .expect("capture terminal fixture")
            .into_canonical_payload();
        TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("validate terminal fixture")
            .restore_inert(config)
            .expect("restore terminal fixture off topology")
    }

    fn test_terminal_config() -> Arc<dyn TerminalConfiguration + Send + Sync> {
        Arc::new(TestTerminalConfig)
    }

    fn capture_record_checkpoint_fixture() -> RecordCheckpointFixture {
        let payload = b"guardian-checkpoint-base".to_vec();
        let (sender, receiver) = sync_channel(1);
        let (staging, mutation_state) = fake_staging([], 1);
        let mut activated = staging
            .activate_verified_restore(
                inert_terminal(),
                Box::new(ChannelGuardianReader { receiver }),
            )
            .expect("activate typed checkpoint fixture reader");
        let pane_id = alloc_pane_id().expect("allocate checkpoint fixture pane id");
        let pane: Arc<dyn Pane> = Arc::new(activated.into_local_pane(
            pane_id,
            0,
            "guardian replay checkpoint fixture".to_string(),
        ));
        let mux = Arc::new(Mux::new(None));
        mux.add_pane(&pane)
            .expect("register checkpoint fixture pane");
        let operation = mux
            .capture_pane_operation(pane_id)
            .expect("capture exact checkpoint fixture registration");

        let directory = tempfile::tempdir().expect("create checkpoint fixture journal directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make checkpoint fixture journal directory private");
        }
        let directory_file = File::open(directory.path()).expect("open fixture journal parent");
        let segment = GuardianOutputSegmentIdentity::new(identity().pane_id(), id(0x500), 1, None)
            .expect("construct checkpoint fixture segment identity");
        let cipher = GuardianOutputCipher::try_from_key_slice(&[0x5a; 32])
            .expect("construct checkpoint fixture cipher");
        let mut journal = GuardianOutputJournal::create_new_at(
            &directory_file,
            std::ffi::OsStr::new("guardian-output.segment"),
            segment,
            cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect("open checkpoint fixture journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate checkpoint fixture journal");
        let receipt = journal
            .append_and_sync(&payload)
            .expect("append checkpoint fixture output");
        sender
            .send((segment, receipt, Arc::<[u8]>::from(payload)))
            .expect("release checkpoint fixture parser delivery");
        let capture = operation
            .capture_live_parser_checkpoint(
                segment,
                receipt,
                TerminalCheckpointLimits::default(),
                Duration::from_secs(5),
            )
            .expect("capture exact live parser checkpoint fixture");
        let descriptor =
            GuardianCheckpointDescriptorV1::from_live_capture(&capture, identity().generation())
                .expect("bind checkpoint fixture descriptor");
        let checkpoint = Zeroizing::new(capture.terminal_checkpoint().canonical_payload().to_vec());
        drop(operation);
        drop(sender);
        mutation_state
            .lock()
            .directives
            .push_back(FakeDirective::Observe(ObservedChildState::Exited(0)));
        RecordCheckpointFixture {
            descriptor,
            checkpoint,
            segment,
            receipt,
        }
    }

    fn checkpoint_and_complete_pages(
        descriptor: GuardianCheckpointDescriptorV1,
        checkpoint: Zeroizing<Vec<u8>>,
    ) -> VecDeque<GuardianReplayPageDelivery> {
        let snapshot_id = id(0x600);
        let snapshot_digest = [0x61; 32];
        let (next_sequence, previous_record_digest) = descriptor
            .suffix_start()
            .expect("record checkpoint has a suffix boundary");
        let cursor = GuardianReplayCursorV1::new(
            snapshot_id,
            snapshot_digest,
            GuardianReplayPhaseV1::Output,
            1,
            0,
            next_sequence,
            previous_record_digest,
            0,
            GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES,
            GUARDIAN_MAX_REPLAY_RECORDS,
        )
        .expect("construct checkpoint fixture continuation cursor");
        let checkpoint_page = GuardianReplayPageDelivery::new(
            identity().pane_id(),
            identity().generation(),
            snapshot_id,
            snapshot_digest,
            [0; 32],
            0,
            Some(cursor),
            GuardianReplayPageBodyDelivery::CheckpointChunk(
                GuardianCheckpointChunkDelivery::new(descriptor, 0, checkpoint)
                    .expect("construct checkpoint fixture delivery"),
            ),
        )
        .expect("construct checkpoint fixture page");
        let GuardianCheckpointOutputBoundaryV1::Record {
            sequence,
            record_digest,
            cumulative_plaintext_bytes,
            ..
        } = descriptor.output_boundary()
        else {
            panic!("checkpoint fixture must be record-backed");
        };
        let complete_page = GuardianReplayPageDelivery::new(
            identity().pane_id(),
            identity().generation(),
            snapshot_id,
            snapshot_digest,
            cursor.digest(),
            1,
            None,
            GuardianReplayPageBodyDelivery::Complete {
                checkpoint_id: descriptor.checkpoint_id(),
                through_sequence: sequence,
                terminal_record_digest: record_digest,
                cumulative_plaintext_bytes,
            },
        )
        .expect("construct checkpoint fixture completion page");
        VecDeque::from([checkpoint_page, complete_page])
    }

    fn tail_output_page(
        fixture: &RecordCheckpointFixture,
        payload: &[u8],
    ) -> GuardianReplayPageDelivery {
        let GuardianCheckpointOutputBoundaryV1::Record {
            sequence,
            record_digest,
            committed_log_bytes,
            cumulative_plaintext_bytes,
            ..
        } = fixture.descriptor.output_boundary()
        else {
            panic!("tail fixture must be record-backed");
        };
        let output_sequence = sequence.checked_add(1).expect("tail sequence advances");
        let output_cumulative = cumulative_plaintext_bytes
            .checked_add(u64::try_from(payload.len()).expect("tail payload length fits u64"))
            .expect("tail cumulative bytes advance");
        let output_log_bytes = committed_log_bytes
            .checked_add(u64::try_from(payload.len()).expect("tail payload length fits u64"))
            .and_then(|bytes| bytes.checked_add(256))
            .expect("tail committed bytes advance");
        let output_digest: [u8; 32] = Sha256::digest(payload).into();
        let metadata = GuardianReplayRecordMetadataV1::new(
            fixture.segment.segment_id(),
            fixture.segment.first_sequence(),
            None,
            output_sequence,
            u32::try_from(payload.len()).expect("tail payload length fits u32"),
            output_cumulative,
            output_log_bytes,
            output_digest,
        )
        .expect("construct tail record metadata");
        let records = GuardianReplayOutputRecordsDelivery::new(
            output_sequence,
            record_digest,
            vec![
                GuardianReplayRecordDelivery::new(metadata, Zeroizing::new(payload.to_vec()))
                    .expect("construct tail record delivery"),
            ],
        )
        .expect("construct tail output page body");
        let snapshot_id = id(0x700);
        let snapshot_digest = [0x71; 32];
        let cursor = GuardianReplayCursorV1::new(
            snapshot_id,
            snapshot_digest,
            GuardianReplayPhaseV1::Output,
            1,
            0,
            output_sequence
                .checked_add(1)
                .expect("tail continuation sequence advances"),
            output_digest,
            0,
            GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES,
            GUARDIAN_MAX_REPLAY_RECORDS,
        )
        .expect("construct tail continuation cursor");
        GuardianReplayPageDelivery::new(
            identity().pane_id(),
            identity().generation(),
            snapshot_id,
            snapshot_digest,
            [0; 32],
            0,
            Some(cursor),
            GuardianReplayPageBodyDelivery::OutputRecords(records),
        )
        .expect("construct tail replay page")
    }

    fn tail_complete_page(
        descriptor: GuardianCheckpointDescriptorV1,
    ) -> GuardianReplayPageDelivery {
        let GuardianCheckpointOutputBoundaryV1::Record {
            sequence,
            record_digest,
            cumulative_plaintext_bytes,
            ..
        } = descriptor.output_boundary()
        else {
            panic!("tail completion fixture must be record-backed");
        };
        GuardianReplayPageDelivery::new(
            identity().pane_id(),
            identity().generation(),
            id(0x710),
            [0x72; 32],
            [0; 32],
            0,
            None,
            GuardianReplayPageBodyDelivery::Complete {
                checkpoint_id: descriptor.checkpoint_id(),
                through_sequence: sequence,
                terminal_record_digest: record_digest,
                cumulative_plaintext_bytes,
            },
        )
        .expect("construct tail completion replay page")
    }

    fn tail_no_recovery_base_gap_page(
        descriptor: GuardianCheckpointDescriptorV1,
    ) -> GuardianReplayPageDelivery {
        let (requested_sequence, _) = descriptor
            .suffix_start()
            .expect("gap fixture must have a suffix boundary");
        GuardianReplayPageDelivery::new(
            identity().pane_id(),
            identity().generation(),
            id(0x720),
            [0x73; 32],
            [0; 32],
            0,
            None,
            GuardianReplayPageBodyDelivery::Gap {
                requested_sequence,
                oldest_retained_sequence: 0,
                verified_through_sequence: 0,
                reason: GuardianReplayGapReasonV1::NoRecoveryBase,
            },
        )
        .expect("construct terminal no-recovery-base gap page")
    }

    fn call_ids(call: &FakeCall) -> Option<(u64, Uuid, Uuid)> {
        match call {
            FakeCall::Input {
                sequence,
                request_id,
                effect_id,
                ..
            }
            | FakeCall::Resize {
                sequence,
                request_id,
                effect_id,
                ..
            }
            | FakeCall::Terminate {
                sequence,
                request_id,
                effect_id,
            }
            | FakeCall::Close {
                sequence,
                request_id,
                effect_id,
            }
            | FakeCall::Retire {
                sequence,
                request_id,
                effect_id,
            } => Some((*sequence, *request_id, *effect_id)),
            FakeCall::QueryInput { .. } | FakeCall::Census => None,
        }
    }

    #[test]
    fn consuming_restore_binds_real_checkpoint_exact_retries_and_tail_ack_after_delivery() {
        let fixture = capture_record_checkpoint_fixture();
        let tail_payload = b"tail";
        let tail_page = tail_output_page(&fixture, tail_payload);
        let mut pages = checkpoint_and_complete_pages(fixture.descriptor, fixture.checkpoint);
        pages.push_back(tail_page);
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages,
            replay_io_failures: 1,
            ack_io_failures: 1,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let (mut staging, _mutation_state) = fake_staging([], 11);
        staging.replay_transport = Some(Box::new(FakeReplayTransport {
            state: Arc::clone(&replay_state),
        }));

        let activated = staging
            .restore_and_activate(test_terminal_config(), TerminalCheckpointLimits::default())
            .expect("consume checkpoint and activate proxy off topology");
        activated
            .terminal
            .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
            .expect("restored terminal remains recovery-ground after activation");
        {
            let state = replay_state.lock();
            assert_eq!(
                state.pages.len(),
                1,
                "tail page stays unread until pane I/O"
            );
            assert_eq!(
                state.acks.len(),
                3,
                "lost checkpoint Ack is retried exactly"
            );
            assert_eq!(state.requests.len(), 3, "lost Replay is retried exactly");
            assert_eq!(state.requests[0].0, state.requests[1].0);
            assert_eq!(state.requests[0].1, state.requests[1].1);
            assert_eq!(state.acks[0].0, state.acks[1].0);
            assert_eq!(state.acks[0].1, state.acks[1].1);
        }

        let mut reader = activated
            .guardian_live_output_reader
            .take()
            .expect("take sole verified record-aware guardian tail reader");
        let acks_before_delivery = replay_state.lock().acks.len();
        assert_eq!(
            acks_before_delivery, 3,
            "tail record is not acknowledged before typed delivery"
        );
        reader
            .deliver_next_record(&mut |segment, output, payload| {
                assert_eq!(segment.durable_pane_id(), identity().pane_id());
                assert_eq!(
                    output.sequence(),
                    fixture
                        .receipt
                        .sequence()
                        .checked_add(1)
                        .expect("fixture tail sequence advances")
                );
                assert_eq!(payload.as_ref(), tail_payload);
                assert_eq!(
                    replay_state.lock().acks.len(),
                    acks_before_delivery,
                    "replay page Ack cannot precede successful parser delivery"
                );
                Ok(())
            })
            .expect("deliver exact typed tail record");
        assert_eq!(
            replay_state.lock().acks.len(),
            acks_before_delivery + 1,
            "the replay page is acknowledged only after typed delivery succeeds"
        );

        let error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("fake source ends after exact tail Ack");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let state = replay_state.lock();
        assert_eq!(state.acks.len(), acks_before_delivery + 1);
        let tail_ack = state.acks.last().expect("tail Ack recorded").1;
        assert_eq!(
            tail_ack.through_sequence(),
            fixture
                .receipt
                .sequence()
                .checked_add(1)
                .expect("fixture tail sequence advances")
        );
        assert!(!tail_ack.release_if_complete());
        assert!(activated.guardian_live_output_reader.is_none());
        assert!(activated.pty.try_clone_reader().is_err());
    }

    #[test]
    fn restore_rejects_checkpoint_pixel_geometry_drift_before_activation() {
        let fixture = capture_record_checkpoint_fixture();
        let limits = TerminalCheckpointLimits::default();
        let mut checkpoint =
            BoundedReplayBuffer::new(limits.max_encoded_bytes).expect("bound checkpoint fixture");
        checkpoint
            .write_all(&fixture.checkpoint)
            .expect("copy checkpoint fixture into bounded restore buffer");
        let mut mismatched_size = size(24, 80);
        mismatched_size.pixel_width = mismatched_size
            .pixel_width
            .checked_add(1)
            .expect("fixture pixel width advances");

        let error = restore_inert_checkpoint(
            fixture.descriptor,
            &checkpoint,
            mismatched_size,
            test_terminal_config(),
            limits,
        )
        .expect_err("pixel geometry drift cannot become a live terminal");
        assert!(matches!(
            error,
            GuardianProxyError::ReplayInvariant(
                "checkpoint pixel geometry does not match the claimed topology manifest"
            )
        ));
    }

    #[test]
    fn unpublished_staging_drop_retires_with_exact_retry_after_lost_reply() {
        let (staging, state) = fake_staging([FakeDirective::Io, FakeDirective::Auto], 81);

        drop(staging);

        let state = state.lock();
        let retire_calls = state
            .calls
            .iter()
            .filter_map(|call| match call {
                FakeCall::Retire {
                    sequence,
                    request_id,
                    effect_id,
                } => Some((*sequence, *request_id, *effect_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retire_calls.len(), 2, "lost retirement reply is retried");
        assert_eq!(retire_calls[0], retire_calls[1]);
        assert_eq!(retire_calls[0].0, 81);
    }

    #[test]
    fn unpublished_activated_proxy_drop_retires_its_claimed_lease_once() {
        let (staging, state) = fake_staging([FakeDirective::Auto], 91);
        let activated = staging.activate_after_inert_restore_for_test(
            inert_terminal(),
            Box::new(io::Cursor::new(Vec::<u8>::new())),
        );

        drop(activated);

        assert!(matches!(
            state.lock().calls.as_slice(),
            [FakeCall::Retire { sequence: 91, .. }]
        ));
    }

    #[test]
    fn tail_completion_ack_survives_lost_reply_across_delivery_calls() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor)
            .expect("derive tail completion boundary");
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_complete_page(fixture.descriptor)]),
            replay_io_failures: 0,
            ack_io_failures: GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut reader = GuardianReplayTailReader::new(
            Box::new(FakeReplayTransport {
                state: Arc::clone(&replay_state),
            }),
            identity(),
            fixture.descriptor.checkpoint_id(),
            boundary,
            TerminalCheckpointLimits::default(),
        )
        .expect("construct exact tail completion reader");
        let first_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("two lost completion Ack replies remain visible");
        assert_eq!(first_error.kind(), io::ErrorKind::ConnectionReset);
        {
            let state = replay_state.lock();
            assert_eq!(state.requests.len(), 1);
            assert_eq!(state.acks.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS);
            assert_eq!(state.acks[0], state.acks[1]);
        }

        let second_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("fake source ends after the retained Ack succeeds");
        assert_eq!(second_error.kind(), io::ErrorKind::InvalidData);
        let state = replay_state.lock();
        assert_eq!(state.acks.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS + 1);
        assert!(state.acks.iter().all(|ack| *ack == state.acks[0]));
    }

    #[test]
    fn tail_gap_releases_snapshot_with_exact_ack_before_surfacing_gap() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor)
            .expect("derive tail gap boundary");
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_no_recovery_base_gap_page(fixture.descriptor)]),
            replay_io_failures: 0,
            ack_io_failures: GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut reader = GuardianReplayTailReader::new(
            Box::new(FakeReplayTransport {
                state: Arc::clone(&replay_state),
            }),
            identity(),
            fixture.descriptor.checkpoint_id(),
            boundary,
            TerminalCheckpointLimits::default(),
        )
        .expect("construct no-recovery-base tail reader");
        let first_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("two lost terminal Ack replies remain visible");
        assert_eq!(first_error.kind(), io::ErrorKind::ConnectionReset);
        {
            let state = replay_state.lock();
            assert_eq!(state.requests.len(), 1);
            assert_eq!(state.acks.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS);
            assert_eq!(state.acks[0], state.acks[1]);
            assert!(state.acks[0].1.release_if_complete());
            assert_eq!(state.acks[0].1.through_sequence(), 0);
            assert_eq!(state.acks[0].1.through_record_digest(), [0; 32]);
        }

        let gap = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("retained terminal Ack succeeds before Gap is surfaced");
        assert_eq!(gap.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            gap.get_ref()
                .and_then(|error| error.downcast_ref::<GuardianProxyError>()),
            Some(GuardianProxyError::ReplayGap)
        ));
        let state = replay_state.lock();
        assert_eq!(
            state.requests.len(),
            1,
            "Gap is not reopened before delivery"
        );
        assert_eq!(state.acks.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS + 1);
        assert!(state.acks.iter().all(|ack| *ack == state.acks[0]));
    }

    #[test]
    fn tail_replay_request_survives_lost_reply_across_delivery_calls() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor)
            .expect("derive lost Replay boundary");
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_output_page(&fixture, b"tail")]),
            replay_io_failures: GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS,
            ack_io_failures: 0,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut reader = GuardianReplayTailReader::new(
            Box::new(FakeReplayTransport {
                state: Arc::clone(&replay_state),
            }),
            identity(),
            fixture.descriptor.checkpoint_id(),
            boundary,
            TerminalCheckpointLimits::default(),
        )
        .expect("construct lost Replay tail reader");
        let first_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("two lost Replay replies remain visible");
        assert_eq!(first_error.kind(), io::ErrorKind::ConnectionReset);
        let mut delivered = None;
        reader
            .deliver_next_record(&mut |_, _, payload| {
                delivered = Some(payload);
                Ok(())
            })
            .expect("retry the retained Replay");
        assert_eq!(delivered.as_deref(), Some(b"tail".as_slice()));

        let state = replay_state.lock();
        assert_eq!(state.requests.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS + 1);
        assert!(
            state
                .requests
                .iter()
                .all(|request| *request == state.requests[0])
        );
        assert_eq!(state.acks.len(), 1, "successful record delivery is acknowledged");
    }

    #[test]
    fn failed_typed_record_delivery_permanently_withholds_replay_ack() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor)
            .expect("derive failed delivery boundary");
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_output_page(&fixture, b"tail")]),
            replay_io_failures: 0,
            ack_io_failures: 0,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut reader = GuardianReplayTailReader::new(
            Box::new(FakeReplayTransport {
                state: Arc::clone(&replay_state),
            }),
            identity(),
            fixture.descriptor.checkpoint_id(),
            boundary,
            TerminalCheckpointLimits::default(),
        )
        .expect("construct failed-delivery tail reader");

        let delivery_error = reader
            .deliver_next_record(&mut |_, _, _| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected parser delivery failure",
                ))
            })
            .expect_err("parser delivery failure must remain visible");
        assert_eq!(delivery_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(
            replay_state.lock().acks.is_empty(),
            "failed parser delivery cannot acknowledge its replay page"
        );

        let terminal_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("failed delivery makes this reader terminal");
        assert_eq!(terminal_error.kind(), io::ErrorKind::InvalidData);
        let state = replay_state.lock();
        assert_eq!(state.requests.len(), 1);
        assert!(state.acks.is_empty());
    }

    #[test]
    fn idle_tail_polling_backs_off_to_the_protocol_wait_ceiling() {
        assert_eq!(
            GUARDIAN_REPLAY_IDLE_POLL_MAX_INTERVAL,
            Duration::from_millis(u64::from(GUARDIAN_MAX_REPLAY_WAIT_MILLIS))
        );
        let mut interval = GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL;
        let observed = (0..7)
            .map(|_| {
                let current = interval;
                interval = next_guardian_idle_poll_interval(interval);
                current
            })
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            [50, 100, 200, 400, 800, 1_000, 1_000]
                .map(Duration::from_millis)
                .to_vec()
        );
    }

    #[test]
    fn server_held_replay_wait_replaces_only_the_consumed_client_backoff() {
        let resume = GuardianReplayRequestV1::Open {
            selector: GuardianReplaySelectorV1::Resume {
                checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes([0x61; 32]).unwrap(),
                next_sequence: 1,
                previous_record_digest: [0; 32],
            },
            max_plaintext_bytes: 4_096,
            max_records: 16,
            wait_millis: GUARDIAN_MAX_REPLAY_WAIT_MILLIS,
        };
        let wait_budget = guardian_replay_server_wait_budget(resume);
        assert_eq!(wait_budget, Duration::from_millis(1_000));
        assert_eq!(
            guardian_replay_remaining_idle_delay(
                Duration::from_millis(50),
                wait_budget,
                Duration::from_millis(20),
            ),
            Duration::from_millis(30),
            "an old or early-returning peer retains the unconsumed client fallback"
        );
        assert_eq!(
            guardian_replay_remaining_idle_delay(
                Duration::from_millis(50),
                wait_budget,
                Duration::from_millis(50),
            ),
            Duration::ZERO,
            "a server-held wait must not be followed by a second idle sleep"
        );

        let legacy_immediate = GuardianReplayRequestV1::Open {
            selector: GuardianReplaySelectorV1::LatestCompatible,
            max_plaintext_bytes: 4_096,
            max_records: 16,
            wait_millis: GUARDIAN_MAX_REPLAY_WAIT_MILLIS,
        };
        assert_eq!(
            guardian_replay_server_wait_budget(legacy_immediate),
            Duration::ZERO,
            "only the server-held Resume contract may consume client backoff"
        );
        assert_eq!(
            guardian_replay_remaining_idle_delay(
                Duration::from_millis(50),
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn replay_client_rejections_preserve_identity_failure_classes() {
        for (rejection, expected) in [
            (
                GuardianRejectionCode::PaneNotFound,
                GuardianProxyError::PaneNotFound,
            ),
            (
                GuardianRejectionCode::GuardianIncarnationMismatch,
                GuardianProxyError::GuardianIncarnationChanged,
            ),
            (
                GuardianRejectionCode::StaleLease,
                GuardianProxyError::LeaseFenced,
            ),
            (
                GuardianRejectionCode::ClaimGenerationMismatch,
                GuardianProxyError::LeaseFenced,
            ),
            (
                GuardianRejectionCode::PaneTerminal,
                GuardianProxyError::LeaseNotAttached,
            ),
        ] {
            let observed = map_replay_client_error(GuardianClientError::Rejected(rejection));
            assert_eq!(
                std::mem::discriminant(&observed),
                std::mem::discriminant(&expected)
            );
            assert_eq!(
                io::Error::from(observed).kind(),
                io::Error::from(expected).kind()
            );
        }
    }

    #[test]
    fn test_only_inert_activation_is_the_only_reader_gate_and_all_facets_share_one_sequence_actor()
    {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Auto,
                FakeDirective::Auto,
                FakeDirective::Auto,
                FakeDirective::Auto,
            ],
            11,
        );
        assert!(
            staging.reader_slot.take_reader().is_err(),
            "no byte source may escape before inert activation"
        );
        let mut activated = staging.activate_after_inert_restore_for_test(
            inert_terminal(),
            Box::new(io::Cursor::new(b"authenticated-tail".to_vec())),
        );
        assert!(
            activated
                .terminal
                .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
                .is_ok(),
            "test-only activation preserves the restored terminal"
        );

        let mut reader = activated
            .pty
            .try_clone_reader()
            .expect("take the one authenticated tail reader");
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .expect("read authenticated tail fixture");
        assert_eq!(bytes, b"authenticated-tail");
        assert!(
            activated.pty.try_clone_reader().is_err(),
            "the consuming replay reader cannot be duplicated"
        );

        activated
            .writer
            .write_all(b"input")
            .expect("write through guardian actor");
        activated
            .pty
            .resize(size(30, 100))
            .expect("resize through guardian actor");
        activated
            .process
            .kill()
            .expect("signal through guardian actor");
        activated
            .lease_control
            .close(activated.lease_identity)
            .expect("close through guardian actor");

        let state = state.lock();
        let identities = state.calls.iter().filter_map(call_ids).collect::<Vec<_>>();
        assert_eq!(
            identities.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            vec![11, 12, 13, 14]
        );
        let request_ids = identities.iter().map(|entry| entry.1).collect::<Vec<_>>();
        let effect_ids = identities.iter().map(|entry| entry.2).collect::<Vec<_>>();
        assert!(request_ids.iter().all(|value| !value.is_nil()));
        assert!(effect_ids.iter().all(|value| !value.is_nil()));
        assert_eq!(
            request_ids
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            request_ids.len()
        );
        assert_eq!(
            effect_ids
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            effect_ids.len()
        );
    }

    #[test]
    fn lost_generic_reply_reuses_exact_sequence_request_and_effect() {
        let (staging, state) = fake_staging(
            [FakeDirective::Io, FakeDirective::Auto, FakeDirective::Auto],
            7,
        );
        let actor = staging.shared_actor();
        let new_size = size(35, 120);
        assert!(matches!(
            actor.lock().resize(new_size),
            Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
        ));
        actor
            .lock()
            .resize(new_size)
            .expect("exact resize retry succeeds");
        actor
            .lock()
            .terminate()
            .expect("successor mutation advances exactly once");

        let state = state.lock();
        let calls = &state.calls;
        assert_eq!(calls.len(), 3);
        assert_eq!(call_ids(&calls[0]), call_ids(&calls[1]));
        assert_eq!(call_ids(&calls[0]).map(|value| value.0), Some(7));
        assert_eq!(call_ids(&calls[2]).map(|value| value.0), Some(8));
    }

    #[test]
    fn lost_input_and_lost_query_reply_reuse_exact_identities_without_plaintext_retention() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Io,
                FakeDirective::Query(InputEffectState::DurableFull),
            ],
            21,
        );
        let actor = staging.shared_actor();
        let payload = b"highly-sensitive-input";
        assert!(matches!(
            actor.lock().write_input(payload),
            Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
        ));
        let pending_debug = format!("{:?}", actor.lock());
        assert!(!pending_debug.contains("highly-sensitive-input"));
        let digest_debug = hex::encode(Sha256::digest(payload));
        assert!(!pending_debug.contains(&digest_debug));

        assert!(matches!(
            actor.lock().write_input(payload),
            Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
        ));
        assert_eq!(
            actor
                .lock()
                .write_input(payload)
                .expect("query proves original input durable"),
            payload.len()
        );

        let state = state.lock();
        let calls = &state.calls;
        assert_eq!(calls.len(), 3);
        let FakeCall::Input {
            sequence,
            request_id: input_request,
            effect_id: input_effect,
            ..
        } = &calls[0]
        else {
            panic!("first call is input");
        };
        assert_eq!(*sequence, 21);
        let FakeCall::QueryInput {
            request_id: first_query,
            effect_id: first_effect,
        } = &calls[1]
        else {
            panic!("second call is query");
        };
        let FakeCall::QueryInput {
            request_id: retry_query,
            effect_id: retry_effect,
        } = &calls[2]
        else {
            panic!("third call is query retry");
        };
        assert_eq!(first_query, retry_query);
        assert_eq!(first_effect, input_effect);
        assert_eq!(retry_effect, input_effect);
        assert!(!input_request.is_nil());
    }

    #[test]
    fn allocation_failure_keeps_input_unsubmitted_and_retries_without_effect_query() {
        let (staging, state) = fake_staging([FakeDirective::Auto], 26);
        let actor = staging.shared_actor();
        actor.lock().fail_next_input_copy = true;
        assert!(matches!(
            actor.lock().write_input(b"allocate-before-submit"),
            Err(GuardianProxyError::InputAllocation)
        ));
        assert!(
            state.lock().calls.is_empty(),
            "allocation precedes transport"
        );
        assert_eq!(
            actor
                .lock()
                .write_input(b"allocate-before-submit")
                .expect("exact input retries directly after allocation failure"),
            22
        );
        assert_eq!(actor.lock().next_sequence(), 27);
        let state = state.lock();
        assert!(matches!(state.calls.as_slice(), [FakeCall::Input { .. }]));
    }

    #[test]
    fn not_seen_input_is_resent_with_the_exact_original_identity() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Query(InputEffectState::NotSeen),
                FakeDirective::Auto,
            ],
            31,
        );
        let actor = staging.shared_actor();
        let payload = b"retry-exactly";
        assert!(actor.lock().write_input(payload).is_err());
        assert_eq!(
            actor
                .lock()
                .write_input(payload)
                .expect("NotSeen authorizes exact resend"),
            payload.len()
        );

        let state = state.lock();
        let calls = &state.calls;
        assert_eq!(calls.len(), 3);
        let first = call_ids(&calls[0]).expect("first input identity");
        let retry = call_ids(&calls[2]).expect("retried input identity");
        assert_eq!(first, retry);
        let (
            FakeCall::Input {
                payload_sha256: first_digest,
                ..
            },
            FakeCall::Input {
                payload_sha256: retry_digest,
                ..
            },
        ) = (&calls[0], &calls[2])
        else {
            panic!("input calls surround the query");
        };
        assert_eq!(first_digest, retry_digest);
    }

    #[test]
    fn not_seen_pending_input_rejects_different_bytes_without_a_second_input() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Query(InputEffectState::NotSeen),
            ],
            41,
        );
        let actor = staging.shared_actor();
        assert!(actor.lock().write_input(b"original").is_err());
        assert!(matches!(
            actor.lock().write_input(b"different"),
            Err(GuardianProxyError::PendingInputPayloadRequired)
        ));
        let state = state.lock();
        assert_eq!(
            state
                .calls
                .iter()
                .filter(|call| matches!(call, FakeCall::Input { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn known_not_applied_consumes_the_sequence_but_never_claims_a_write() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Reject(GuardianRejectionCode::InputKnownNotApplied),
                FakeDirective::Auto,
            ],
            51,
        );
        let actor = staging.shared_actor();
        assert!(matches!(
            actor.lock().write_input(b"again"),
            Err(GuardianProxyError::InputKnownNotApplied)
        ));
        assert_eq!(actor.lock().next_sequence(), 52);
        assert_eq!(
            actor
                .lock()
                .write_input(b"again")
                .expect("new effect may retry zero-applied input"),
            5
        );
        let state = state.lock();
        let calls = &state.calls;
        let first = call_ids(&calls[0]).expect("first input identity");
        let second = call_ids(&calls[1]).expect("successor input identity");
        assert_eq!(first.0, 51);
        assert_eq!(second.0, 52);
        assert_ne!(first.1, second.1);
        assert_ne!(first.2, second.2);
    }

    #[test]
    fn flush_reports_the_exact_durable_prefix_instead_of_silently_settling_input() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Query(InputEffectState::DurablePrefix { applied_bytes: 3 }),
            ],
            56,
        );
        let actor = staging.shared_actor();
        assert!(matches!(
            actor.lock().write_input(b"abcdef"),
            Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
        ));
        assert!(matches!(
            actor.lock().flush_pending(),
            Err(GuardianProxyError::PreviousInputPartiallyApplied {
                applied_bytes: 3,
                input_bytes: 6,
            })
        ));
        assert_eq!(actor.lock().next_sequence(), 57);
        let state = state.lock();
        let calls = &state.calls;
        assert!(matches!(
            calls.as_slice(),
            [FakeCall::Input { .. }, FakeCall::QueryInput { .. }]
        ));
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ExpectedRejection {
        PaneNotFound,
        GuardianIncarnationChanged,
        Fenced,
        Closed,
        Indeterminate,
    }

    #[test]
    fn terminal_rejections_fence_close_or_quarantine_without_endless_retry() {
        let cases = [
            (
                GuardianRejectionCode::PaneNotFound,
                ExpectedRejection::PaneNotFound,
            ),
            (
                GuardianRejectionCode::GuardianIncarnationMismatch,
                ExpectedRejection::GuardianIncarnationChanged,
            ),
            (GuardianRejectionCode::StaleLease, ExpectedRejection::Fenced),
            (
                GuardianRejectionCode::ClaimGenerationMismatch,
                ExpectedRejection::Fenced,
            ),
            (
                GuardianRejectionCode::PaneTerminal,
                ExpectedRejection::Closed,
            ),
            (
                GuardianRejectionCode::CheckpointOutcomeIndeterminate,
                ExpectedRejection::Indeterminate,
            ),
        ];
        for (code, expected) in cases {
            let (staging, state) = fake_staging([FakeDirective::Reject(code)], 81);
            let actor = staging.shared_actor();
            let first = actor.lock().resize(size(25, 81));
            assert!(
                match expected {
                    ExpectedRejection::PaneNotFound => {
                        matches!(&first, Err(GuardianProxyError::PaneNotFound))
                    }
                    ExpectedRejection::GuardianIncarnationChanged =>
                        matches!(&first, Err(GuardianProxyError::GuardianIncarnationChanged)),
                    ExpectedRejection::Fenced => {
                        matches!(&first, Err(GuardianProxyError::LeaseFenced))
                    }
                    ExpectedRejection::Closed => {
                        matches!(&first, Err(GuardianProxyError::LeaseNotAttached))
                    }
                    ExpectedRejection::Indeterminate => matches!(
                        &first,
                        Err(GuardianProxyError::MutationOutcomeIndeterminate)
                    ),
                },
                "unexpected first classification for {code:?}: {first:?}"
            );
            let second = actor.lock().resize(size(25, 81));
            assert!(
                match expected {
                    ExpectedRejection::PaneNotFound
                    | ExpectedRejection::GuardianIncarnationChanged
                    | ExpectedRejection::Fenced => {
                        matches!(&second, Err(GuardianProxyError::LeaseFenced))
                    }
                    ExpectedRejection::Closed => {
                        matches!(&second, Err(GuardianProxyError::LeaseNotAttached))
                    }
                    ExpectedRejection::Indeterminate => {
                        matches!(&second, Err(GuardianProxyError::PaneQuarantined))
                    }
                },
                "terminal rejection {code:?} issued an exact retry: {second:?}"
            );
            assert_eq!(state.lock().calls.len(), 1, "terminal rejection {code:?}");
        }
    }

    #[test]
    fn invariant_identity_and_sequence_rejections_quarantine_without_endless_retry() {
        let codes = [
            GuardianRejectionCode::InvalidRequest,
            GuardianRejectionCode::PaneAlreadyExists,
            GuardianRejectionCode::RequestIdentityConflict,
            GuardianRejectionCode::EffectIdentityConflict,
            GuardianRejectionCode::RepeatedSequence,
            GuardianRejectionCode::SequenceGap,
            GuardianRejectionCode::GenerationExhausted,
            GuardianRejectionCode::SequenceExhausted,
            GuardianRejectionCode::InputDurabilityIdentityMismatch,
            GuardianRejectionCode::CensusSnapshotNotFound,
            GuardianRejectionCode::CensusSnapshotIdentityConflict,
            GuardianRejectionCode::InvalidCensusCursor,
            GuardianRejectionCode::InternalInvariant,
            GuardianRejectionCode::CheckpointIdentityMismatch,
            GuardianRejectionCode::OwnedPanesPresent,
            GuardianRejectionCode::InputKnownNotApplied,
        ];
        for code in codes {
            let (staging, state) = fake_staging([FakeDirective::Reject(code)], 91);
            let actor = staging.shared_actor();
            assert!(matches!(
                actor.lock().resize(size(26, 82)),
                Err(GuardianProxyError::Client(GuardianClientError::Rejected(
                    rejected
                ))) if rejected == code
            ));
            assert!(matches!(
                actor.lock().resize(size(26, 82)),
                Err(GuardianProxyError::PaneQuarantined)
            ));
            assert_eq!(state.lock().calls.len(), 1, "quarantined code {code:?}");
        }
    }

    #[test]
    fn retryable_rejections_preserve_exact_pending_identity_and_would_block_semantics() {
        for code in [
            GuardianRejectionCode::CapacityExhausted,
            GuardianRejectionCode::RequestAliasCapacityExhausted,
        ] {
            let (staging, state) =
                fake_staging([FakeDirective::Reject(code), FakeDirective::Auto], 101);
            let actor = staging.shared_actor();
            assert!(matches!(
                actor.lock().resize(size(27, 83)),
                Err(GuardianProxyError::Client(GuardianClientError::Rejected(
                    rejected
                ))) if rejected == code
            ));
            actor
                .lock()
                .resize(size(27, 83))
                .expect("capacity rejection permits one exact retry");
            let state = state.lock();
            let calls = &state.calls;
            assert_eq!(calls.len(), 2);
            assert_eq!(call_ids(&calls[0]), call_ids(&calls[1]));
        }

        let (staging, state) = fake_staging(
            [
                FakeDirective::Reject(GuardianRejectionCode::InputDurabilityPending),
                FakeDirective::Auto,
            ],
            111,
        );
        let actor = staging.shared_actor();
        let pending = actor
            .lock()
            .resize(size(28, 84))
            .expect_err("durability pending is not success");
        assert!(matches!(
            &pending,
            GuardianProxyError::InputDurabilityPending
        ));
        assert_eq!(io::Error::from(pending).kind(), io::ErrorKind::WouldBlock);
        actor
            .lock()
            .resize(size(28, 84))
            .expect("durability pending permits one exact retry");
        let state = state.lock();
        let calls = &state.calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(call_ids(&calls[0]), call_ids(&calls[1]));
    }

    #[test]
    fn expired_replay_snapshot_requires_durable_restore_without_retrying_the_same_request() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Reject(GuardianRejectionCode::ReplaySnapshotExpired),
                FakeDirective::Auto,
            ],
            116,
        );
        let actor = staging.shared_actor();
        let expired = actor
            .lock()
            .resize(size(29, 85))
            .expect_err("expired replay snapshot is not a successful mutation");
        assert!(matches!(
            &expired,
            GuardianProxyError::ReplaySnapshotExpired
        ));
        assert_eq!(io::Error::from(expired).kind(), io::ErrorKind::WouldBlock);
        assert!(matches!(
            actor.lock().resize(size(29, 85)),
            Err(GuardianProxyError::ReplaySnapshotExpired)
        ));
        assert_eq!(
            state.lock().calls.len(),
            1,
            "snapshot expiry reopens durable restore instead of retrying the exact request"
        );
    }

    #[test]
    fn shared_census_serves_multiple_panes_once_and_never_blocks_mutation_authority() {
        let live_identity = identity_for(id(11), 4);
        let exited_identity = identity_for(id(12), 7);
        let fenced_identity = identity_for(id(13), 10);
        let stale_fenced_entry = observed_census_entry(
            identity_for(fenced_identity.pane_id(), fenced_identity.generation() - 1),
            ObservedChildState::Running,
        );
        let census_state = Arc::new(Mutex::new(ScriptedCensusState {
            calls: 0,
            snapshots: VecDeque::from([
                vec![
                    observed_census_entry(live_identity, ObservedChildState::Running),
                    observed_census_entry(exited_identity, ObservedChildState::Exited(23)),
                    stale_fenced_entry,
                ],
                vec![observed_census_entry(
                    live_identity,
                    ObservedChildState::Exited(31),
                )],
            ]),
        }));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let census = Arc::new(
            GuardianCensusCoordinator::with_transport(
                identity().guardian_incarnation(),
                identity().mux_incarnation(),
                Duration::from_secs(60),
                Box::new(ScriptedCensusTransport {
                    state: Arc::clone(&census_state),
                    entered_once: Some(entered_tx),
                    release_once: Some(release_rx),
                }),
            )
            .expect("construct shared scripted census coordinator"),
        );
        let mutation_state = Arc::new(Mutex::new(FakeState::default()));
        let stage = |identity, next_sequence| {
            GuardianProxyStaging::with_transports(
                identity,
                next_sequence,
                size(24, 80),
                Box::new(FakeTransport {
                    state: Arc::clone(&mutation_state),
                }),
                Arc::clone(&census),
            )
            .expect("stage pane with shared census coordinator")
        };
        let live = stage(live_identity, 201);
        let exited = stage(exited_identity, 301);
        let fenced = stage(fenced_identity, 401);
        let live_actor = live.shared_actor();
        let mut live_child = GuardianProxyChild {
            actor: Arc::clone(&live_actor),
            census: Arc::clone(&census),
        };
        let live_observer = thread::spawn(move || live_child.try_wait());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shared census entered its one fleet network walk");

        let (mutation_tx, mutation_rx) = sync_channel(1);
        let mutation_actor = Arc::clone(&live_actor);
        let mutation_thread = thread::spawn(move || {
            mutation_tx
                .send(mutation_actor.lock().write_input(b"x"))
                .expect("report concurrent mutation result");
        });
        assert_eq!(
            mutation_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("fleet census must not own the pane mutation mutex")
                .expect("concurrent input succeeds"),
            1
        );
        release_tx.send(()).expect("release shared fleet census");
        assert!(
            live_observer
                .join()
                .expect("join live observer")
                .expect("observe live pane")
                .is_none()
        );
        mutation_thread.join().expect("join concurrent mutation");

        let mut exited_child = GuardianProxyChild {
            actor: exited.shared_actor(),
            census: Arc::clone(&census),
        };
        assert_eq!(
            exited_child
                .try_wait()
                .expect("read terminal row from shared cache")
                .expect("pane is terminal")
                .exit_code(),
            23,
            "terminal status survives the shared snapshot exactly"
        );
        let fenced_actor = fenced.shared_actor();
        let mut fenced_child = GuardianProxyChild {
            actor: Arc::clone(&fenced_actor),
            census: Arc::clone(&census),
        };
        assert_eq!(
            fenced_child
                .try_wait()
                .expect_err("stale generation must fail closed")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(matches!(
            fenced_actor.lock().flush_pending(),
            Err(GuardianProxyError::LeaseFenced)
        ));
        assert_eq!(
            census_state.lock().calls,
            1,
            "three pane observations share one fleet census"
        );

        census.invalidate();
        let mut refreshed_live_child = GuardianProxyChild {
            actor: Arc::clone(&live_actor),
            census: Arc::clone(&census),
        };
        assert_eq!(
            refreshed_live_child
                .try_wait()
                .expect("explicit invalidation refreshes the census")
                .expect("refreshed pane is terminal")
                .exit_code(),
            31
        );
        assert_eq!(census_state.lock().calls, 2);
        assert_eq!(live_actor.lock().next_sequence(), 202);
        assert!(matches!(
            mutation_state.lock().calls.as_slice(),
            [FakeCall::Input { sequence: 201, .. }]
        ));
    }

    #[test]
    fn staging_rejects_a_census_coordinator_from_another_guardian_or_mux() {
        let coordinator = Arc::new(
            GuardianCensusCoordinator::with_transport(
                identity().guardian_incarnation(),
                identity().mux_incarnation(),
                GUARDIAN_CENSUS_CACHE_MAX_AGE,
                Box::new(FakeCensusTransport {
                    state: Arc::new(Mutex::new(FakeState::default())),
                    identity: identity(),
                }),
            )
            .expect("construct bound census coordinator"),
        );
        let mismatches = [
            GuardianPaneLeaseIdentity::new(id(14), id(99), identity().mux_incarnation(), 1)
                .expect("valid mismatched guardian identity"),
            GuardianPaneLeaseIdentity::new(id(15), identity().guardian_incarnation(), id(100), 1)
                .expect("valid mismatched mux identity"),
        ];
        for mismatch in mismatches {
            let result = GuardianProxyStaging::with_transports(
                mismatch,
                1,
                size(24, 80),
                Box::new(FakeTransport {
                    state: Arc::new(Mutex::new(FakeState::default())),
                }),
                Arc::clone(&coordinator),
            );
            assert!(matches!(
                result,
                Err(GuardianProxyError::LeaseIdentityMismatch)
            ));
        }
    }

    #[test]
    fn child_observation_is_byte_silent_and_does_not_consume_mutation_sequence() {
        let (staging, state) =
            fake_staging([FakeDirective::Observe(ObservedChildState::Exited(23))], 61);
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        let status = child
            .try_wait()
            .expect("observe guardian child")
            .expect("child exited");
        assert_eq!(status.exit_code(), 23);
        assert_eq!(actor.lock().next_sequence(), 61);
        assert!(matches!(
            actor.lock().flush_pending(),
            Err(GuardianProxyError::LeaseNotAttached)
        ));
        actor
            .lock()
            .retire(identity())
            .expect("terminal census already proves the live lease absent");
        assert_eq!(state.lock().calls, vec![FakeCall::Census]);
    }

    #[test]
    fn terminal_observation_preserves_pending_input_recovery_until_exact_disposition_is_known() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Observe(ObservedChildState::Exited(24)),
                FakeDirective::Query(InputEffectState::DurablePrefix { applied_bytes: 3 }),
            ],
            64,
        );
        let actor = staging.shared_actor();
        assert!(matches!(
            actor.lock().write_input(b"abcdef"),
            Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
        ));
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        assert_eq!(
            child
                .try_wait()
                .expect("terminal census remains observable")
                .expect("pane exited")
                .exit_code(),
            24
        );
        assert!(matches!(
            actor.lock().flush_pending(),
            Err(GuardianProxyError::PreviousInputPartiallyApplied {
                applied_bytes: 3,
                input_bytes: 6,
            })
        ));
        assert_eq!(actor.lock().next_sequence(), 65);
        assert!(matches!(
            actor.lock().flush_pending(),
            Err(GuardianProxyError::LeaseNotAttached)
        ));
        assert!(matches!(
            state.lock().calls.as_slice(),
            [
                FakeCall::Input { .. },
                FakeCall::Census,
                FakeCall::QueryInput { .. }
            ]
        ));
    }

    #[test]
    fn evicted_census_snapshot_reopens_once_without_quarantining_the_pane() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Reject(GuardianRejectionCode::CensusSnapshotNotFound),
                FakeDirective::Observe(ObservedChildState::Exited(17)),
            ],
            62,
        );
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        assert_eq!(
            child
                .try_wait()
                .expect("evicted snapshot reopens from cursor zero")
                .expect("pane exited")
                .exit_code(),
            17
        );
        assert_eq!(actor.lock().next_sequence(), 62);
        assert_eq!(state.lock().calls, vec![FakeCall::Census, FakeCall::Census]);
    }

    #[test]
    fn newly_terminal_mutation_disposition_invalidates_a_cached_running_row() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Observe(ObservedChildState::Running),
                FakeDirective::Reject(GuardianRejectionCode::PaneTerminal),
                FakeDirective::Observe(ObservedChildState::Exited(19)),
            ],
            63,
        );
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        assert!(child.try_wait().expect("prime live census cache").is_none());
        assert!(matches!(
            actor.lock().resize(size(25, 81)),
            Err(GuardianProxyError::LeaseNotAttached)
        ));
        assert_eq!(
            child
                .try_wait()
                .expect("closed disposition forces a fresh census")
                .expect("fresh row is terminal")
                .exit_code(),
            19
        );
        assert!(matches!(
            state.lock().calls.as_slice(),
            [FakeCall::Census, FakeCall::Resize { .. }, FakeCall::Census]
        ));
    }

    #[test]
    fn terminal_census_classifier_rejects_missing_exit_status_for_every_terminal_row() {
        for status in [
            GuardianCensusPaneStatus::ExitedUnclaimed,
            GuardianCensusPaneStatus::ClosedTerminal,
        ] {
            let entry = GuardianCensusEntry {
                pane_id: identity().pane_id(),
                status,
                generation: identity().generation(),
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                indeterminate_checkpoint_effect: None,
                exit_status: None,
                quarantine_reason: None,
            };
            assert!(matches!(
                classify_child_census_entry(identity(), entry),
                Err(GuardianMutationTransportError::ChildExitStatusUnavailable)
            ));
        }
    }

    #[test]
    fn census_classifier_rejects_a_row_for_another_pane_even_at_the_same_generation() {
        let entry = observed_census_entry(
            identity_for(id(99), identity().generation()),
            ObservedChildState::Running,
        );
        assert!(matches!(
            classify_child_census_entry(identity(), entry),
            Err(GuardianMutationTransportError::LeaseMismatch)
        ));
    }

    #[test]
    fn terminal_census_without_exit_status_fails_closed_instead_of_claiming_success() {
        let (staging, state) = fake_staging([FakeDirective::ObserveMissingExitStatus], 62);
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        let error = child
            .try_wait()
            .expect_err("missing terminal exit status cannot become exit code zero");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "guardian terminal census row omitted its exit status"
        );
        assert!(matches!(
            child.try_wait(),
            Err(error) if error.kind() == io::ErrorKind::Other
        ));
        assert_eq!(
            state.lock().calls,
            vec![FakeCall::Census],
            "quarantine prevents repeated census traffic"
        );
    }

    #[test]
    fn blocked_paginated_observation_never_blocks_the_mutation_actor() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let census = Arc::new(
            GuardianCensusCoordinator::with_transport(
                identity().guardian_incarnation(),
                identity().mux_incarnation(),
                GUARDIAN_CENSUS_CACHE_MAX_AGE,
                Box::new(BlockingCensusTransport {
                    identity: identity(),
                    entered: entered_tx,
                    release: release_rx,
                }),
            )
            .expect("construct blocking census coordinator"),
        );
        let staging = GuardianProxyStaging::with_transports(
            identity(),
            121,
            size(24, 80),
            Box::new(FakeTransport {
                state: Arc::clone(&state),
            }),
            Arc::clone(&census),
        )
        .expect("stage proxy with blocked observer");
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census,
        };
        let observer_thread = thread::spawn(move || child.try_wait());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observer entered its network wait");

        let (mutation_tx, mutation_rx) = sync_channel(1);
        let mutation_actor = Arc::clone(&actor);
        let mutation_thread = thread::spawn(move || {
            mutation_tx
                .send(mutation_actor.lock().write_input(b"x"))
                .expect("report mutation result");
        });
        assert_eq!(
            mutation_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mutation must complete while observation is blocked")
                .expect("mutation succeeds"),
            1
        );
        release_tx.send(()).expect("release blocked observer");
        assert!(
            observer_thread
                .join()
                .expect("join observer thread")
                .expect("observe child")
                .is_none()
        );
        mutation_thread.join().expect("join mutation thread");
        assert_eq!(actor.lock().next_sequence(), 122);
        assert!(matches!(
            state.lock().calls.as_slice(),
            [FakeCall::Input { .. }]
        ));
    }

    #[test]
    fn successful_observation_is_revalidated_after_concurrent_lease_retirement() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let census = Arc::new(
            GuardianCensusCoordinator::with_transport(
                identity().guardian_incarnation(),
                identity().mux_incarnation(),
                GUARDIAN_CENSUS_CACHE_MAX_AGE,
                Box::new(BlockingCensusTransport {
                    identity: identity(),
                    entered: entered_tx,
                    release: release_rx,
                }),
            )
            .expect("construct blocking census coordinator"),
        );
        let staging = GuardianProxyStaging::with_transports(
            identity(),
            131,
            size(24, 80),
            Box::new(FakeTransport {
                state: Arc::clone(&state),
            }),
            Arc::clone(&census),
        )
        .expect("stage proxy for observation race");
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census,
        };
        let observer_thread = thread::spawn(move || child.try_wait());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observer entered its network wait");
        actor
            .lock()
            .retire(identity())
            .expect("retire lease while observation is in flight");
        release_tx.send(()).expect("release successful observation");
        let error = observer_thread
            .join()
            .expect("join observer thread")
            .expect_err("pre-retirement Running result must not escape");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(
            state.lock().calls.as_slice(),
            [FakeCall::Retire { sequence: 131, .. }]
        ));
    }

    #[test]
    fn production_staging_reader_remains_fail_closed() {
        let (staging, state) = fake_staging([], 71);
        assert!(staging.reader_slot.take_reader().is_err());
        assert!(staging.reader_slot.take_reader().is_err());
        assert_eq!(state.lock().calls.as_slice(), &[]);
    }
}
