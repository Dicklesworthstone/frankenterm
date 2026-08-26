//! Concrete mux-side proxy objects for one already-claimed guardian pane.
//!
//! This module deliberately stops short of production activation. It owns the
//! mutation-sequence actor and the portable-pty object implementations, but it
//! does not claim panes, fetch replay pages, construct a replay-tail reader, or
//! publish a [`mux::LocalPane`]. The reader slot remains fail-closed in
//! production. A future consuming replay coordinator must validate and bind the
//! checkpoint, raw suffix, final output sequence/digest witness, and live
//! subscription before it may add the sole activation path.

use frankenterm_pty_guardian::{GuardianClient, GuardianClientError};
use mux::guardian_protocol::{
    GUARDIAN_MAX_INPUT_BYTES, GUARDIAN_MAX_PANES, GuardianCensusEntry, GuardianCensusPaneStatus,
    GuardianInputEffectQuery, GuardianRejectionCode, GuardianReply, InputEffectState,
};
use mux::localpane::{GuardianPaneLeaseControl, GuardianPaneLeaseIdentity};
use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_CENSUS_REFRESH_ATTEMPTS: usize = 2;

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
            | GuardianProxyError::ReplaySnapshotExpired => {
                Self::new(io::ErrorKind::WouldBlock, error)
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
            Some(PendingMutation::Input(_)) | Some(PendingMutation::Generic(_)) | None => {
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

enum GuardianReplayReaderState {
    Staged,
    #[cfg(test)]
    Ready(Option<Box<dyn Read + Send>>),
    Taken,
}

impl fmt::Debug for GuardianReplayReaderState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Staged => formatter.write_str("Staged"),
            #[cfg(test)]
            Self::Ready(Some(_)) => formatter.write_str("Ready(Some(<reader>))"),
            #[cfg(test)]
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
            #[cfg(test)]
            GuardianReplayReaderState::Ready(Some(reader)) => Ok(reader),
            #[cfg(test)]
            GuardianReplayReaderState::Ready(None)
            | GuardianReplayReaderState::Staged
            | GuardianReplayReaderState::Taken => {
                *state = prior;
                Err(GuardianProxyError::InvalidConfiguration(
                    "guardian replay reader is unavailable before exact restore activation or after its single take",
                ))
            }
            #[cfg(not(test))]
            GuardianReplayReaderState::Staged | GuardianReplayReaderState::Taken => {
                *state = prior;
                Err(GuardianProxyError::InvalidConfiguration(
                    "guardian replay reader is unavailable before exact restore activation or after its single take",
                ))
            }
        }
    }

    #[cfg(test)]
    fn install_after_test_restore(
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
}

/// A connected, already-claimed guardian lease that is still off topology.
///
/// No portable-pty object and no reader activation method are exposed from this
/// type. Production activation stays absent until the consuming replay
/// coordinator can bind terminal restoration to its authenticated final
/// sequence/digest and live-subscription witness.
pub struct GuardianProxyStaging {
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
    reader_slot: Arc<GuardianReplayReaderSlot>,
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
            .finish_non_exhaustive()
    }
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
        let mutation_transport =
            GuardianClientTransport::connect(socket_path, token_path, identity)?;
        Self::with_transports(
            identity,
            next_sequence,
            size,
            Box::new(mutation_transport),
            census,
        )
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
        Ok(Self {
            actor,
            census,
            reader_slot: Arc::new(GuardianReplayReaderSlot::new()),
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

    #[cfg(test)]
    fn activate_after_inert_restore_for_test(
        self,
        inert_terminal: frankenterm_term::InertTerminal,
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
            lease_identity: identity,
        }
    }
}

#[cfg(test)]
struct TestActivatedGuardianProxy {
    terminal: frankenterm_term::Terminal,
    process: Box<dyn Child + Send>,
    pty: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    lease_control: Arc<dyn GuardianPaneLeaseControl>,
    lease_identity: GuardianPaneLeaseIdentity,
}

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
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::terminalstate::checkpoint::{
        TerminalCheckpointLimits, TerminalCheckpointV2,
    };
    use frankenterm_term::{InertTerminal, Terminal, TerminalConfiguration, TerminalSize};
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

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
                state.snapshots.pop_front().ok_or_else(|| {
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
            pane_id,
            identity().guardian_incarnation(),
            identity().mux_incarnation(),
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
        let config: Arc<dyn TerminalConfiguration + Send + Sync> = Arc::new(TestTerminalConfig);
        let terminal = Terminal::new(
            TerminalSize::default(),
            Arc::clone(&config),
            "FrankenTerm",
            "guardian-proxy-test",
            Box::new(Vec::<u8>::new()),
        );
        let limits = TerminalCheckpointLimits::default();
        let canonical = TerminalCheckpointV2::capture_with_limits(&terminal, limits)
            .expect("capture terminal fixture")
            .to_canonical_json(limits)
            .expect("encode terminal fixture");
        TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("validate terminal fixture")
            .restore_inert(config)
            .expect("restore terminal fixture off topology")
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
            activated.terminal.checkpoint().is_ok(),
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
        let digest_debug = format!("{:x}", Sha256::digest(payload));
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
        assert!(state.lock().calls.is_empty());
    }
}
