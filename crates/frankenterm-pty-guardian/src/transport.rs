//! Bounded authenticated Unix transport for the standalone PTY guardian.

use crate::output::GuardianOutputPipeline;
use crate::runtime::{
    GuardianInputRoute, GuardianInputSubmission, GuardianRuntime,
    GuardianRuntimeConfig, GuardianRuntimeCounters,
    GuardianRuntimeInputCompletionState,
};
use mio::net::{UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Token, Waker};
use mux::guardian_protocol::{
    AuthenticatedGuardianRequest, GuardianCensusPageRequest, GuardianInputEffectQuery,
    GuardianOperation, GuardianProtocolError, GuardianReply, GuardianRequestEnvelope,
    GuardianRequestHeader, GuardianRejectionCode, GuardianResponseEnvelope,
    GuardianResponseStatus, GuardianSecret, GuardianResizePayload, GuardianSignal,
    GuardianSpawnPayload, InputEffectState, GUARDIAN_AUTH_TOKEN_BYTES,
    GUARDIAN_MAX_CENSUS_BYTES, GUARDIAN_MAX_CENSUS_ENTRIES, GUARDIAN_MAX_FRAME_BYTES,
    GUARDIAN_MAX_PANES, decode_guardian_request, decode_guardian_response,
    encode_guardian_request, encode_guardian_response,
};
use nix::unistd::geteuid;
use portable_pty::{CommandBuilder, PtySize};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Seek, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream as BlockingUnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

const LISTENER_TOKEN: Token = Token(0);
const MAX_CONNECTIONS: usize = 1024;
const MAX_OUTPUT_BYTES_PER_PANE: usize = 64 * 1024 * 1024;
const MAX_TOTAL_OUTPUT_BYTES: usize = 1024 * 1024 * 1024;
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
const AUTHENTICATION_DEADLINE: Duration = Duration::from_secs(5);
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_STAGE_COMMIT_MAGIC: [u8; 4] = *b"FTGC";
const TOKEN_STAGE_COMMIT_BYTES: usize = TOKEN_STAGE_COMMIT_MAGIC.len() + 32;

fn partition_endpoint_tokens(max_connections: usize) -> Option<(Token, usize)> {
    let output_completion_token = Token(max_connections.checked_add(1)?);
    let first_pty_token = output_completion_token.0.checked_add(1)?;
    Some((output_completion_token, first_pty_token))
}

/// Fail-closed startup configuration for one guardian process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianServiceConfig {
    socket_path: PathBuf,
    token_path: PathBuf,
    max_connections: usize,
    max_panes: usize,
    // Resident plaintext reservation limits before encrypted append+sync.
    // Cumulative durable output is governed by the journal's separate caps.
    max_output_bytes_per_pane: usize,
    max_total_output_bytes: usize,
    poll_interval: Duration,
}

impl GuardianServiceConfig {
    pub fn new(
        socket_path: PathBuf,
        token_path: PathBuf,
        max_connections: usize,
        max_panes: usize,
        max_output_bytes_per_pane: usize,
        max_total_output_bytes: usize,
        poll_interval: Duration,
    ) -> Result<Self, GuardianServiceError> {
        if max_connections == 0 || max_connections > MAX_CONNECTIONS {
            return Err(GuardianServiceError::InvalidConfiguration(
                "max_connections is outside its supported nonzero bound",
            ));
        }
        if max_panes == 0 || max_panes > GUARDIAN_MAX_PANES {
            return Err(GuardianServiceError::InvalidConfiguration(
                "max_panes is outside the guardian protocol bound",
            ));
        }
        if max_output_bytes_per_pane == 0
            || max_output_bytes_per_pane > MAX_OUTPUT_BYTES_PER_PANE
        {
            return Err(GuardianServiceError::InvalidConfiguration(
                "max_output_bytes_per_pane is outside its supported nonzero bound",
            ));
        }
        let possible_output_bytes = max_panes
            .checked_mul(max_output_bytes_per_pane)
            .ok_or(GuardianServiceError::InvalidConfiguration(
                "configured pane output capacity overflows the platform",
            ))?;
        if max_total_output_bytes == 0
            || max_total_output_bytes > MAX_TOTAL_OUTPUT_BYTES
            || max_total_output_bytes > possible_output_bytes
        {
            return Err(GuardianServiceError::InvalidConfiguration(
                "max_total_output_bytes is outside its supported aggregate bound",
            ));
        }
        if poll_interval.is_zero() || poll_interval > MAX_POLL_INTERVAL {
            return Err(GuardianServiceError::InvalidConfiguration(
                "poll_interval must be between one nanosecond and one second",
            ));
        }
        validate_absolute_path(&socket_path)?;
        validate_absolute_path(&token_path)?;
        if socket_path == token_path {
            return Err(GuardianServiceError::InvalidConfiguration(
                "socket and token paths must differ",
            ));
        }
        Ok(Self {
            socket_path,
            token_path,
            max_connections,
            max_panes,
            max_output_bytes_per_pane,
            max_total_output_bytes,
            poll_interval,
        })
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[derive(Debug, Error)]
pub enum GuardianServiceError {
    #[error("invalid guardian service configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("guardian filesystem security check failed: {0}")]
    FilesystemSecurity(&'static str),
    #[error("guardian service I/O failed at {site}")]
    Io {
        site: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("guardian protocol initialization failed")]
    Protocol(#[from] GuardianProtocolError),
    #[error("guardian durable output initialization failed")]
    OutputInitialization,
    #[error("guardian token entropy acquisition failed")]
    Entropy(#[from] getrandom::Error),
}

impl GuardianServiceError {
    fn io(site: &'static str, source: std::io::Error) -> Self {
        Self::Io { site, source }
    }
}

#[derive(Debug, Error)]
pub enum GuardianClientError {
    #[error("guardian client setup failed")]
    Setup(#[from] GuardianServiceError),
    #[error("guardian client I/O failed")]
    Io(#[from] std::io::Error),
    #[error("guardian client protocol failed")]
    Protocol(#[from] GuardianProtocolError),
    #[error("guardian request was rejected with {0:?}")]
    Rejected(GuardianRejectionCode),
    #[error("guardian returned a reply for the wrong operation")]
    UnexpectedReply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionTokenOutcome {
    Created,
    Existing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianProbeReport {
    pub guardian_incarnation: Uuid,
    pub pane_count: u64,
}

#[derive(Clone, Copy)]
struct ReadyEvent {
    token: Token,
    readable: bool,
    writable: bool,
    closed: bool,
}

struct Connection {
    stream: UnixStream,
    generation: u64,
    read_buf: Zeroizing<Vec<u8>>,
    write_buf: Zeroizing<Vec<u8>>,
    write_offset: usize,
    mux_incarnation: Option<Uuid>,
    close_after_write: bool,
    guarded_stop_response: Option<GuardedStopAuthority>,
    pending_input: Option<GuardianInputRoute>,
    accepted_at: Instant,
}

impl Connection {
    fn new(stream: UnixStream, generation: u64) -> Self {
        Self {
            stream,
            generation,
            read_buf: Zeroizing::new(Vec::new()),
            write_buf: Zeroizing::new(Vec::new()),
            write_offset: 0,
            mux_incarnation: None,
            close_after_write: false,
            guarded_stop_response: None,
            pending_input: None,
            accepted_at: Instant::now(),
        }
    }

    fn identity(&self, token: Token) -> ConnectionIdentity {
        ConnectionIdentity {
            token,
            generation: self.generation,
        }
    }
}

/// Exact identity of one accepted connection-token lifetime.
///
/// Generation fences token recycling only. It is deliberately never compared
/// across distinct connections to decide lifecycle order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ConnectionIdentity {
    token: Token,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingMuxRetirementObservation {
    mux_incarnation: Uuid,
    disconnect_observation_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MuxConnectionTrackingError {
    InvalidIdentity,
    CapacityExhausted,
    ObservationEpochExhausted,
    StaleConnection,
    ConnectionAlreadyAuthenticated,
    MembershipMismatch,
    ActiveMembershipAtReplay,
    PendingRetirementMismatch,
    Poisoned,
}

/// Readiness-loop-owned connection lifecycle and retirement authority.
///
/// `live_connections` is the exact authenticated-membership source of truth,
/// keyed by token plus that token's current generation. Observation epochs are
/// allocated only as lifecycle events are processed by this single owner, so a
/// delayed valid Hello orders after an already-processed disconnect regardless
/// of which connection happened to be accepted first.
struct MuxConnectionTracker {
    live_connections: HashMap<ConnectionIdentity, Option<Uuid>>,
    pending_retirements: Vec<PendingMuxRetirementObservation>,
    max_connections: usize,
    observation_epoch: u64,
    poisoned: bool,
}

impl MuxConnectionTracker {
    fn new(max_connections: usize) -> Result<Self, GuardianServiceError> {
        if max_connections == 0 || max_connections > MAX_CONNECTIONS {
            return Err(GuardianServiceError::InvalidConfiguration(
                "mux connection tracker capacity is invalid",
            ));
        }
        let mut live_connections = HashMap::new();
        live_connections.try_reserve(max_connections).map_err(|_| {
            GuardianServiceError::InvalidConfiguration(
                "mux connection tracker allocation failed",
            )
        })?;
        let mut pending_retirements = Vec::new();
        pending_retirements
            .try_reserve_exact(max_connections)
            .map_err(|_| {
                GuardianServiceError::InvalidConfiguration(
                    "mux retirement tracker allocation failed",
                )
            })?;
        Ok(Self {
            live_connections,
            pending_retirements,
            max_connections,
            observation_epoch: 0,
            poisoned: false,
        })
    }

    fn next_observation_epoch(&mut self) -> Result<u64, MuxConnectionTrackingError> {
        let Some(epoch) = self.observation_epoch.checked_add(1) else {
            self.poisoned = true;
            return Err(MuxConnectionTrackingError::ObservationEpochExhausted);
        };
        self.observation_epoch = epoch;
        Ok(epoch)
    }

    fn require_healthy(&self) -> Result<(), MuxConnectionTrackingError> {
        if self.poisoned {
            Err(MuxConnectionTrackingError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn observe_accept(
        &mut self,
        identity: ConnectionIdentity,
    ) -> Result<(), MuxConnectionTrackingError> {
        self.require_healthy()?;
        if identity.token == LISTENER_TOKEN || identity.generation == 0 {
            return Err(MuxConnectionTrackingError::InvalidIdentity);
        }
        if self.live_connections.contains_key(&identity)
            || self
                .live_connections
                .keys()
                .any(|live| live.token == identity.token)
        {
            return Err(MuxConnectionTrackingError::StaleConnection);
        }
        if self.live_connections.len() >= self.max_connections {
            return Err(MuxConnectionTrackingError::CapacityExhausted);
        }
        self.next_observation_epoch()?;
        self.live_connections.insert(identity, None);
        Ok(())
    }

    fn observe_authenticated_hello(
        &mut self,
        identity: ConnectionIdentity,
        mux_incarnation: Uuid,
    ) -> Result<(), MuxConnectionTrackingError> {
        self.require_healthy()?;
        if mux_incarnation.is_nil() {
            return Err(MuxConnectionTrackingError::InvalidIdentity);
        }
        match self.live_connections.get(&identity) {
            Some(None) => {}
            Some(Some(_)) => {
                return Err(MuxConnectionTrackingError::ConnectionAlreadyAuthenticated);
            }
            None => return Err(MuxConnectionTrackingError::StaleConnection),
        }
        let hello_observation_epoch = self.next_observation_epoch()?;
        self.live_connections
            .insert(identity, Some(mux_incarnation));
        self.pending_retirements.retain(|retirement| {
            retirement.mux_incarnation != mux_incarnation
                || retirement.disconnect_observation_epoch >= hello_observation_epoch
        });
        Ok(())
    }

    fn observe_disconnect(
        &mut self,
        identity: ConnectionIdentity,
        connection_mux_incarnation: Option<Uuid>,
    ) -> Result<(), MuxConnectionTrackingError> {
        self.require_healthy()?;
        let Some(tracked_mux_incarnation) = self.live_connections.get(&identity).copied() else {
            return Err(MuxConnectionTrackingError::StaleConnection);
        };
        if tracked_mux_incarnation != connection_mux_incarnation {
            self.poisoned = true;
            return Err(MuxConnectionTrackingError::MembershipMismatch);
        }
        let disconnect_observation_epoch = self.next_observation_epoch()?;
        self.live_connections.remove(&identity);
        let Some(mux_incarnation) = tracked_mux_incarnation else {
            return Ok(());
        };
        if self.has_authenticated_membership(mux_incarnation) {
            return Ok(());
        }
        if let Some(retirement) = self
            .pending_retirements
            .iter_mut()
            .find(|retirement| retirement.mux_incarnation == mux_incarnation)
        {
            retirement.disconnect_observation_epoch = disconnect_observation_epoch;
            return Ok(());
        }
        if self.pending_retirements.len() >= self.max_connections {
            self.poisoned = true;
            return Err(MuxConnectionTrackingError::CapacityExhausted);
        }
        self.pending_retirements
            .push(PendingMuxRetirementObservation {
                mux_incarnation,
                disconnect_observation_epoch,
            });
        Ok(())
    }

    fn has_authenticated_membership(&self, mux_incarnation: Uuid) -> bool {
        self.live_connections
            .values()
            .any(|membership| *membership == Some(mux_incarnation))
    }

    fn next_replayable_retirement(
        &mut self,
    ) -> Result<Option<PendingMuxRetirementObservation>, MuxConnectionTrackingError> {
        self.require_healthy()?;
        let Some(retirement) = self.pending_retirements.first().copied() else {
            return Ok(None);
        };
        if self.has_authenticated_membership(retirement.mux_incarnation) {
            self.poisoned = true;
            return Err(MuxConnectionTrackingError::ActiveMembershipAtReplay);
        }
        Ok(Some(retirement))
    }

    fn complete_retirement(
        &mut self,
        completed: PendingMuxRetirementObservation,
    ) -> Result<(), MuxConnectionTrackingError> {
        self.require_healthy()?;
        let Some(position) = self
            .pending_retirements
            .iter()
            .position(|pending| *pending == completed)
        else {
            self.poisoned = true;
            return Err(MuxConnectionTrackingError::PendingRetirementMismatch);
        };
        self.pending_retirements.remove(position);
        Ok(())
    }
}

enum FrameProcessing {
    Response(GuardianResponseEnvelope),
    PendingInput,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardedStopAuthority {
    connection: Token,
    request_id: Uuid,
    effect_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketPathIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
}

struct SocketPathAuthority {
    parent: File,
    socket_path: PathBuf,
    leaf_name: OsString,
    identity: SocketPathIdentity,
}

impl SocketPathAuthority {
    fn validate(&self) -> Result<(), GuardianServiceError> {
        validate_pinned_private_parent(&self.socket_path, &self.parent)?;
        let observed = socket_path_identity_at(&self.parent, &self.leaf_name)?;
        if observed != self.identity {
            return Err(GuardianServiceError::FilesystemSecurity(
                "guardian socket path no longer names the bound listener authority",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GuardianLifecycle {
    #[default]
    Running,
    Draining(GuardedStopAuthority),
    ExitReady,
}

impl GuardianLifecycle {
    const fn request_fence(self) -> Result<(), GuardianRejectionCode> {
        if matches!(self, Self::Running) {
            Ok(())
        } else {
            Err(GuardianRejectionCode::InvalidRequest)
        }
    }

    fn begin_guarded_stop(
        &mut self,
        authority: GuardedStopAuthority,
        owned_panes: usize,
    ) -> Result<(), GuardianRejectionCode> {
        if *self != Self::Running {
            return Err(GuardianRejectionCode::InvalidRequest);
        }
        if owned_panes != 0 {
            return Err(GuardianRejectionCode::OwnedPanesPresent);
        }
        *self = Self::Draining(authority);
        Ok(())
    }

    fn response_flushed(&mut self, authority: GuardedStopAuthority) -> bool {
        if *self != Self::Draining(authority) {
            return false;
        }
        *self = Self::ExitReady;
        true
    }

    fn authority_disconnected(&mut self, token: Token) {
        if matches!(*self, Self::Draining(authority) if authority.connection == token) {
            *self = Self::Running;
        }
    }
}

/// Foreground, single-readiness-loop guardian service.
pub struct GuardianService {
    poll: Poll,
    events: Events,
    ready: Vec<ReadyEvent>,
    listener: UnixListener,
    socket_authority: SocketPathAuthority,
    secret: GuardianSecret,
    runtime: GuardianRuntime,
    connections: HashMap<Token, Connection>,
    mux_connections: MuxConnectionTracker,
    free_connection_tokens: Vec<usize>,
    poll_interval: Duration,
    transport_failures: u64,
    lifecycle: GuardianLifecycle,
    output_completion_token: Token,
    next_connection_generation: u64,
}

impl GuardianService {
    pub fn bind(config: GuardianServiceConfig) -> Result<Self, GuardianServiceError> {
        validate_private_parent(&config.socket_path)?;
        validate_private_parent(&config.token_path)?;
        let socket_parent = open_private_parent(&config.socket_path)?;
        let socket_name = config.socket_path.file_name().ok_or(
            GuardianServiceError::InvalidConfiguration(
                "guardian socket path has no file name",
            ),
        )?;
        require_absent_at(&socket_parent, socket_name)?;
        let secret = load_guardian_secret(&config.token_path)?;

        let poll = Poll::new().map_err(|error| GuardianServiceError::io("poll-create", error))?;
        // Connection tokens are recycled, while PTY tokens are deliberately
        // monotonic for the lifetime of the guardian so a queued readiness
        // event cannot acquire a new pane identity. Keep the fixed completion
        // waker below that monotonic range; placing it `max_panes` above the
        // first PTY would collide after ordinary pane churn.
        let (output_completion_token, first_pty_token) =
            partition_endpoint_tokens(config.max_connections).ok_or(
                GuardianServiceError::InvalidConfiguration("PTY token space overflow"),
            )?;
        let runtime_config = GuardianRuntimeConfig::new(
            config.max_panes,
            config.max_output_bytes_per_pane,
            config.max_total_output_bytes,
            first_pty_token,
        )?;
        let output_completion_waker = Arc::new(
            Waker::new(poll.registry(), output_completion_token)
                .map_err(|error| GuardianServiceError::io("output-waker-create", error))?,
        );
        let output_pipeline = GuardianOutputPipeline::open(
            &config.token_path,
            config.max_panes,
            Arc::clone(&output_completion_waker),
        )
        .map_err(|_| GuardianServiceError::OutputInitialization)?;
        let runtime = GuardianRuntime::new(
            poll.registry()
                .try_clone()
                .map_err(|error| GuardianServiceError::io("registry-clone", error))?,
            runtime_config,
            Uuid::new_v4(),
            output_pipeline,
            output_completion_waker,
        )?;
        let endpoint_capacity = config
            .max_connections
            .checked_add(config.max_panes)
            .and_then(|value| value.checked_add(2))
            .ok_or(GuardianServiceError::InvalidConfiguration(
                "event capacity overflow",
            ))?;

        let mut ready = Vec::new();
        ready
            .try_reserve(endpoint_capacity)
            .map_err(|_| GuardianServiceError::InvalidConfiguration("event allocation failed"))?;
        let mut free_connection_tokens = Vec::new();
        free_connection_tokens
            .try_reserve(config.max_connections)
            .map_err(|_| {
                GuardianServiceError::InvalidConfiguration("connection token allocation failed")
            })?;
        free_connection_tokens.extend((1..=config.max_connections).rev());
        let mut connections = HashMap::new();
        connections.try_reserve(config.max_connections).map_err(|_| {
            GuardianServiceError::InvalidConfiguration("connection map allocation failed")
        })?;
        let mux_connections = MuxConnectionTracker::new(config.max_connections)?;
        let events = Events::with_capacity(endpoint_capacity);

        // Private output-directory/key provisioning is the earlier deliberate
        // startup mutation. Socket binding occurs only after that authority and
        // all endpoint allocations exist. A post-bind permission or
        // registration failure deliberately leaves the socket in place for
        // operator inspection; this process never unlinks an existing path.
        let mut listener = UnixListener::bind(&config.socket_path)
            .map_err(|error| GuardianServiceError::io("socket-bind", error))?;
        chmod_socket_at(&socket_parent, socket_name)?;
        validate_pinned_private_parent(&config.socket_path, &socket_parent)?;
        let socket_identity = socket_path_identity_at(&socket_parent, socket_name)?;
        prove_socket_path_routes_to_listener(&mut listener, &config.socket_path)?;
        let socket_authority = SocketPathAuthority {
            parent: socket_parent,
            socket_path: config.socket_path.clone(),
            leaf_name: socket_name.to_os_string(),
            identity: socket_identity,
        };
        socket_authority.validate()?;
        poll.registry()
            .register(&mut listener, LISTENER_TOKEN, Interest::READABLE)
            .map_err(|error| GuardianServiceError::io("listener-register", error))?;

        Ok(Self {
            poll,
            events,
            ready,
            listener,
            socket_authority,
            secret,
            runtime,
            connections,
            mux_connections,
            free_connection_tokens,
            poll_interval: config.poll_interval,
            transport_failures: 0,
            lifecycle: GuardianLifecycle::Running,
            output_completion_token,
            next_connection_generation: 1,
        })
    }

    #[must_use]
    pub fn incarnation(&self) -> Uuid {
        self.runtime.incarnation()
    }

    #[must_use]
    pub const fn transport_failures(&self) -> u64 {
        self.transport_failures
    }

    /// Content-free runtime counters suitable for operator health checks.
    #[must_use]
    pub const fn runtime_counters(&self) -> GuardianRuntimeCounters {
        self.runtime.counters()
    }

    pub fn run_forever(&mut self) -> Result<(), GuardianServiceError> {
        while self.lifecycle != GuardianLifecycle::ExitReady {
            self.poll_once()?;
        }
        Ok(())
    }

    /// Test-support loop with an external stop flag.
    ///
    /// The flag is deliberately powerless while any runtime pane or retained
    /// unjournaled transcript remains owned. Production uses `run_forever` and
    /// can exit only through the authenticated guarded-stop transaction.
    pub fn run_until(&mut self, stop: &AtomicBool) -> Result<(), GuardianServiceError> {
        loop {
            if self.lifecycle == GuardianLifecycle::ExitReady
                || (self.lifecycle == GuardianLifecycle::Running
                    && stop.load(Ordering::Acquire)
                    && self.runtime.pane_count() == 0)
            {
                break;
            }
            self.poll_once()?;
        }
        Ok(())
    }

    /// Integration-test cleanup loop with an unconditional emergency-abort
    /// signal in addition to the ordinary ownership-aware stop flag.
    ///
    /// The abort signal is deliberately separate from [`Self::run_until`]: it
    /// must never be mistaken for a production guarded-stop transition or proof
    /// that owned pane state was durably retired. Its only purpose is to let a
    /// test's RAII guard join the service thread after an assertion failure.
    #[doc(hidden)]
    pub fn run_until_with_test_abort(
        &mut self,
        stop: &AtomicBool,
        abort: &AtomicBool,
    ) -> Result<(), GuardianServiceError> {
        loop {
            if abort.load(Ordering::Acquire)
                || self.lifecycle == GuardianLifecycle::ExitReady
                || (self.lifecycle == GuardianLifecycle::Running
                    && stop.load(Ordering::Acquire)
                    && self.runtime.pane_count() == 0)
            {
                break;
            }
            self.poll_once()?;
        }
        Ok(())
    }

    pub fn poll_once(&mut self) -> Result<(), GuardianServiceError> {
        if self.lifecycle == GuardianLifecycle::ExitReady {
            return Ok(());
        }
        self.socket_authority.validate()?;
        match self.poll.poll(&mut self.events, Some(self.poll_interval)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(()),
            Err(error) => return Err(GuardianServiceError::io("readiness-poll", error)),
        }

        self.ready.clear();
        for event in &self.events {
            self.ready.push(ReadyEvent {
                token: event.token(),
                readable: event.is_readable(),
                writable: event.is_writable(),
                closed: event.is_error() || event.is_read_closed() || event.is_write_closed(),
            });
        }

        for index in 0..self.ready.len() {
            let event = self.ready[index];
            if event.token == LISTENER_TOKEN {
                if event.readable {
                    self.accept_connections()?;
                }
            } else if event.token == self.output_completion_token {
                // Drain worker completions only after every connection event
                // in this readiness batch. A ready authenticated Hello must
                // publish its lifecycle observation before deferred retirement
                // can replay from a co-ready input completion.
                continue;
            } else if self.runtime.owns_pty_token(event.token) {
                if event.readable || event.closed {
                    self.runtime.handle_pty_ready(event.token);
                }
            } else {
                self.drive_connection(event);
            }
        }
        // Waker notifications may coalesce. A nonblocking drain after every
        // readiness batch makes completion application independent of the
        // number of worker wake calls represented by one poll event.
        self.runtime.handle_output_completions();
        self.handle_input_completions();
        // Replay only after every connection event in this readiness batch and
        // every available input-authority restoration has been observed.  In
        // particular, `finish_connection` must only queue: replaying from the
        // middle of the loop above could retire a mux before a co-ready delayed
        // Hello publishes its later lifecycle-observation epoch.
        self.replay_deferred_mux_retirements();
        self.expire_unauthenticated_connections();
        self.runtime.reap_children_once();
        Ok(())
    }

    fn accept_connections(&mut self) -> Result<(), GuardianServiceError> {
        self.socket_authority.validate()?;
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.transport_failures = self.transport_failures.saturating_add(1);
                    break;
                }
            };
            self.socket_authority.validate()?;
            if self.lifecycle != GuardianLifecycle::Running {
                continue;
            }
            let Some(raw_token) = self.free_connection_tokens.pop() else {
                self.transport_failures = self.transport_failures.saturating_add(1);
                continue;
            };
            let generation = self.next_connection_generation;
            let Some(next_generation) = generation.checked_add(1) else {
                self.free_connection_tokens.push(raw_token);
                self.transport_failures = self.transport_failures.saturating_add(1);
                continue;
            };
            let token = Token(raw_token);
            let identity = ConnectionIdentity { token, generation };
            if self
                .poll
                .registry()
                .register(&mut stream, token, Interest::READABLE)
                .is_err()
            {
                self.free_connection_tokens.push(raw_token);
                self.transport_failures = self.transport_failures.saturating_add(1);
                continue;
            }
            if self.mux_connections.observe_accept(identity).is_err() {
                let _ = self.poll.registry().deregister(&mut stream);
                self.free_connection_tokens.push(raw_token);
                self.transport_failures = self.transport_failures.saturating_add(1);
                continue;
            }
            self.next_connection_generation = next_generation;
            self.connections
                .insert(token, Connection::new(stream, generation));
        }
        Ok(())
    }

    fn drive_connection(&mut self, event: ReadyEvent) {
        let Some(mut connection) = self.connections.remove(&event.token) else {
            return;
        };
        if connection.pending_input.is_some() {
            let keep = monitor_pending_input_connection(&mut connection, event);
            if keep {
                self.connections.insert(event.token, connection);
            } else {
                self.finish_connection(event.token, connection);
            }
            return;
        }
        let mut keep = !event.closed;
        if keep && event.readable && connection.write_buf.is_empty() {
            keep = self.read_connection_frame(event.token, &mut connection);
        }
        if keep && event.writable && !connection.write_buf.is_empty() {
            keep = self.write_connection_frame(event.token, &mut connection);
        }

        if keep {
            self.connections.insert(event.token, connection);
        } else {
            self.finish_connection(event.token, connection);
        }
    }

    fn read_connection_frame(&mut self, token: Token, connection: &mut Connection) -> bool {
        loop {
            let mut chunk = Zeroizing::new([0_u8; 8192]);
            match connection.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(count) => {
                    let Some(next_len) = connection.read_buf.len().checked_add(count) else {
                        return false;
                    };
                    if next_len > GUARDIAN_MAX_FRAME_BYTES
                        || connection.read_buf.try_reserve(count).is_err()
                    {
                        return false;
                    }
                    connection.read_buf.extend_from_slice(&chunk[..count]);
                    let Some(frame_len) = complete_frame_len(&connection.read_buf) else {
                        continue;
                    };
                    if frame_len > GUARDIAN_MAX_FRAME_BYTES
                        || connection.read_buf.len() > frame_len
                    {
                        return false;
                    }
                    if connection.read_buf.len() == frame_len {
                        let frame = std::mem::replace(
                            &mut connection.read_buf,
                            Zeroizing::new(Vec::new()),
                        );
                        return match self.process_frame(token, connection, &frame) {
                            FrameProcessing::Response(response) => {
                                connection.write_buf =
                                    match encode_guardian_response(&self.secret, &response) {
                                        Ok(frame) => Zeroizing::new(frame),
                                        Err(_) => return false,
                                    };
                                connection.write_offset = 0;
                                self.poll
                                    .registry()
                                    .reregister(
                                        &mut connection.stream,
                                        token,
                                        Interest::WRITABLE,
                                    )
                                    .is_ok()
                            }
                            FrameProcessing::PendingInput => self
                                .poll
                                .registry()
                                .reregister(
                                    &mut connection.stream,
                                    token,
                                    Interest::READABLE,
                                )
                                .is_ok(),
                            FrameProcessing::Close => false,
                        };
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }

    fn write_connection_frame(&mut self, token: Token, connection: &mut Connection) -> bool {
        while connection.write_offset < connection.write_buf.len() {
            match connection
                .stream
                .write(&connection.write_buf[connection.write_offset..])
            {
                Ok(0) => return false,
                Ok(count) => connection.write_offset += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        loop {
            match connection.stream.flush() {
                Ok(()) => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
        connection.write_buf.zeroize();
        connection.write_offset = 0;
        if let Some(authority) = connection.guarded_stop_response.take() {
            if !self.lifecycle.response_flushed(authority) {
                self.transport_failures = self.transport_failures.saturating_add(1);
                return false;
            }
            return false;
        }
        if connection.close_after_write {
            return false;
        }
        self.poll
            .registry()
            .reregister(&mut connection.stream, token, Interest::READABLE)
            .is_ok()
    }

    fn process_frame(
        &mut self,
        token: Token,
        connection: &mut Connection,
        frame: &[u8],
    ) -> FrameProcessing {
        let Ok(request) = decode_guardian_request(&self.secret, frame) else {
            return FrameProcessing::Close;
        };
        self.process_authenticated_frame(token, connection, request)
    }

    fn process_authenticated_frame(
        &mut self,
        token: Token,
        connection: &mut Connection,
        mut request: AuthenticatedGuardianRequest,
    ) -> FrameProcessing {
        if self.lifecycle.request_fence().is_err() {
            connection.close_after_write = true;
            let response = GuardianResponseEnvelope::rejection(
                &request,
                GuardianRejectionCode::InvalidRequest,
            );
            request.zeroize_payload();
            return FrameProcessing::Response(response);
        }
        match connection.mux_incarnation {
            None => {
                if request.header().operation != GuardianOperation::Hello {
                    request.zeroize_payload();
                    return FrameProcessing::Close;
                }
                let Some(response) = self.runtime.dispatch(&request) else {
                    request.zeroize_payload();
                    return FrameProcessing::Close;
                };
                if response.header().status == GuardianResponseStatus::Success {
                    let mux_incarnation = request.header().mux_incarnation;
                    if self
                        .mux_connections
                        .observe_authenticated_hello(
                            connection.identity(token),
                            mux_incarnation,
                        )
                        .is_err()
                    {
                        self.transport_failures =
                            self.transport_failures.saturating_add(1);
                        request.zeroize_payload();
                        return FrameProcessing::Close;
                    }
                    connection.mux_incarnation = Some(mux_incarnation);
                } else {
                    connection.close_after_write = true;
                }
                request.zeroize_payload();
                FrameProcessing::Response(response)
            }
            Some(mux_incarnation) => {
                if request.header().operation == GuardianOperation::Hello
                    || request.header().mux_incarnation != mux_incarnation
                {
                    connection.close_after_write = true;
                    let response = GuardianResponseEnvelope::rejection(
                        &request,
                        GuardianRejectionCode::InvalidRequest,
                    );
                    request.zeroize_payload();
                    return FrameProcessing::Response(response);
                }
                if request.header().operation == GuardianOperation::GuardedStop {
                    let Some(effect_id) = request.header().effect_id else {
                        request.zeroize_payload();
                        return FrameProcessing::Close;
                    };
                    let authority = GuardedStopAuthority {
                        connection: token,
                        request_id: request.header().request_id,
                        effect_id,
                    };
                    if let Err(code) = self
                        .lifecycle
                        .begin_guarded_stop(authority, self.runtime.pane_count())
                    {
                        let response =
                            GuardianResponseEnvelope::rejection(&request, code);
                        request.zeroize_payload();
                        return FrameProcessing::Response(response);
                    }
                    connection.guarded_stop_response = Some(authority);
                    connection.close_after_write = true;
                    let response = GuardianResponseEnvelope::reply(
                        &request,
                        &GuardianReply::GuardedStopAccepted,
                    );
                    request.zeroize_payload();
                    return match response {
                        Ok(response) => FrameProcessing::Response(response),
                        Err(_) => FrameProcessing::Close,
                    };
                }
                if request.header().operation == GuardianOperation::Input {
                    let Some(route) = request.header().effect_id.and_then(|effect_id| {
                        GuardianInputRoute::new(
                            token,
                            connection.generation,
                            request.header().request_id,
                            effect_id,
                        )
                    }) else {
                        let response = GuardianResponseEnvelope::rejection(
                            &request,
                            GuardianRejectionCode::InvalidRequest,
                        );
                        request.zeroize_payload();
                        return FrameProcessing::Response(response);
                    };
                    return match self.runtime.submit_input(request, route) {
                        GuardianInputSubmission::Pending => {
                            connection.pending_input = Some(route);
                            FrameProcessing::PendingInput
                        }
                        GuardianInputSubmission::Respond(response) => {
                            FrameProcessing::Response(response)
                        }
                        GuardianInputSubmission::CloseRetryably => FrameProcessing::Close,
                    };
                }
                let response = self.runtime.dispatch(&request);
                request.zeroize_payload();
                match response {
                    Some(response) => FrameProcessing::Response(response),
                    None => FrameProcessing::Close,
                }
            }
        }
    }

    fn handle_input_completions(&mut self) {
        loop {
            let completion = match self.runtime.try_input_completion() {
                GuardianRuntimeInputCompletionState::Ready(completion) => completion,
                GuardianRuntimeInputCompletionState::Empty
                | GuardianRuntimeInputCompletionState::Disconnected => break,
            };
            let token = completion.route.connection_token;
            let Some(mut connection) = self.connections.remove(&token) else {
                // The originating peer disconnected. Runtime restoration and
                // deferred final-lease retirement already happened before this
                // routing step; never redirect the result to a recycled token.
                continue;
            };
            if !pending_input_route_matches(
                connection.generation,
                connection.pending_input,
                completion.route,
            ) {
                self.connections.insert(token, connection);
                continue;
            }
            connection.pending_input = None;
            let Some(response) = completion.response else {
                self.finish_connection(token, connection);
                continue;
            };
            let Ok(frame) = encode_guardian_response(&self.secret, &response) else {
                self.finish_connection(token, connection);
                continue;
            };
            connection.write_buf = Zeroizing::new(frame);
            connection.write_offset = 0;
            if self
                .poll
                .registry()
                .reregister(&mut connection.stream, token, Interest::WRITABLE)
                .is_ok()
            {
                self.connections.insert(token, connection);
            } else {
                self.finish_connection(token, connection);
            }
        }
    }

    fn finish_connection(&mut self, token: Token, mut connection: Connection) {
        let _ = self.poll.registry().deregister(&mut connection.stream);
        self.lifecycle.authority_disconnected(token);
        let identity = connection.identity(token);
        let observed = self
            .mux_connections
            .observe_disconnect(identity, connection.mux_incarnation);
        self.free_connection_tokens.push(token.0);
        if observed.is_err() {
            self.transport_failures = self.transport_failures.saturating_add(1);
            return;
        }
    }

    /// Replay only a readiness-loop-owned final-disconnect observation whose
    /// exact mux membership is still empty at the last possible moment.
    fn replay_deferred_mux_retirements(&mut self) {
        loop {
            let retirement = match self.mux_connections.next_replayable_retirement() {
                Ok(Some(retirement)) => retirement,
                Ok(None) => return,
                Err(_) => {
                    self.transport_failures = self.transport_failures.saturating_add(1);
                    return;
                }
            };
            let result = match self
                .runtime
                .retire_disconnected_mux(retirement.mux_incarnation)
            {
                Ok(Some(result)) => result,
                Ok(None) => return,
                Err(_) => {
                    self.transport_failures = self.transport_failures.saturating_add(1);
                    return;
                }
            };
            if result.pending_input_panes != 0
                || result.indeterminate_checkpoint_panes != 0
            {
                return;
            }
            if self
                .mux_connections
                .complete_retirement(retirement)
                .is_err()
            {
                self.transport_failures = self.transport_failures.saturating_add(1);
                return;
            }
        }
    }

    fn expire_unauthenticated_connections(&mut self) {
        let now = Instant::now();
        self.ready.clear();
        for (token, connection) in &self.connections {
            if connection.mux_incarnation.is_none()
                && now.saturating_duration_since(connection.accepted_at)
                    >= AUTHENTICATION_DEADLINE
            {
                self.ready.push(ReadyEvent {
                    token: *token,
                    readable: false,
                    writable: false,
                    closed: true,
                });
            }
        }
        for index in 0..self.ready.len() {
            let token = self.ready[index].token;
            if let Some(connection) = self.connections.remove(&token) {
                self.finish_connection(token, connection);
            }
        }
    }
}

fn monitor_pending_input_connection(
    connection: &mut Connection,
    event: ReadyEvent,
) -> bool {
    if event.closed {
        return false;
    }
    if !event.readable {
        return true;
    }
    // One connection carries one request at a time. Readability while its
    // input is pending therefore means EOF or forbidden pipelining; either way
    // close the transport identity while the worker safely finishes the exact
    // durable disposition.
    let mut probe = Zeroizing::new([0_u8; 1]);
    loop {
        match connection.stream.read(&mut probe[..]) {
            Ok(_) => return false,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

fn pending_input_route_matches(
    connection_generation: u64,
    pending_input: Option<GuardianInputRoute>,
    completion: GuardianInputRoute,
) -> bool {
    connection_generation == completion.connection_generation
        && matches!(pending_input, Some(pending) if pending == completion)
}

/// Blocking client used by mux integration and real-service lifetime tests.
pub struct GuardianClient {
    stream: BlockingUnixStream,
    secret: GuardianSecret,
    mux_incarnation: Uuid,
    guardian_incarnation: Uuid,
    #[cfg(test)]
    request_wipe_probe: Option<Arc<ClientRequestWipeProbe>>,
}

#[cfg(test)]
#[derive(Default)]
struct ClientRequestWipeProbe {
    explicit_wipe: AtomicBool,
    drop_wipe: AtomicBool,
    authenticated_input_wipe: AtomicBool,
    encoded_frame_wipe: AtomicBool,
}

/// Owned client request whose plaintext is retired on every encoding exit.
struct OwnedClientRequest {
    request: GuardianRequestEnvelope,
    #[cfg(test)]
    wipe_probe: Option<Arc<ClientRequestWipeProbe>>,
}

impl OwnedClientRequest {
    fn new(request: GuardianRequestEnvelope) -> Self {
        Self {
            request,
            #[cfg(test)]
            wipe_probe: None,
        }
    }

    #[cfg(test)]
    fn set_wipe_probe(&mut self, probe: Option<Arc<ClientRequestWipeProbe>>) {
        self.wipe_probe = probe;
    }

    fn envelope(&self) -> &GuardianRequestEnvelope {
        &self.request
    }

    fn zeroize_payload(&mut self) {
        self.request.zeroize_payload();
        #[cfg(test)]
        if let Some(probe) = self.wipe_probe.as_ref() {
            probe.explicit_wipe.store(
                self.request.payload().is_empty(),
                Ordering::SeqCst,
            );
        }
    }
}

impl Drop for OwnedClientRequest {
    fn drop(&mut self) {
        self.request.zeroize_payload();
        #[cfg(test)]
        if let Some(probe) = self.wipe_probe.as_ref() {
            probe.drop_wipe.store(
                self.request.payload().is_empty(),
                Ordering::SeqCst,
            );
        }
    }
}

impl GuardianClient {
    pub fn connect(
        socket_path: &Path,
        token_path: &Path,
        mux_incarnation: Uuid,
    ) -> Result<Self, GuardianClientError> {
        if mux_incarnation.is_nil() {
            return Err(GuardianClientError::Protocol(
                GuardianProtocolError::ZeroIdentity("mux incarnation"),
            ));
        }
        validate_absolute_path(socket_path)?;
        validate_private_parent(socket_path)?;
        validate_existing_socket(socket_path)?;
        let secret = load_guardian_secret(token_path)?;
        let stream = BlockingUnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
        let mut client = Self {
            stream,
            secret,
            mux_incarnation,
            guardian_incarnation: Uuid::nil(),
            #[cfg(test)]
            request_wipe_probe: None,
        };
        let reply = client.exchange(GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Hello,
                Uuid::nil(),
                mux_incarnation,
                Uuid::new_v4(),
                None,
                0,
                0,
                None,
                &[],
            ),
            Vec::new(),
        ))?;
        let GuardianReply::Hello {
            guardian_incarnation,
        } = reply
        else {
            return Err(GuardianClientError::UnexpectedReply);
        };
        client.guardian_incarnation = guardian_incarnation;
        Ok(client)
    }

    #[must_use]
    pub const fn guardian_incarnation(&self) -> Uuid {
        self.guardian_incarnation
    }

    pub fn spawn(
        &mut self,
        pane_id: Uuid,
        request_id: Uuid,
        effect_id: Uuid,
        command: CommandBuilder,
        size: PtySize,
    ) -> Result<GuardianReply, GuardianClientError> {
        let payload = GuardianSpawnPayload::new(command, size)?.encode()?;
        let request = self.request(
            GuardianOperation::Spawn,
            request_id,
            Some(pane_id),
            0,
            0,
            Some(effect_id),
            payload,
        );
        self.exchange(request)
    }

    pub fn claim(
        &mut self,
        pane_id: Uuid,
        observed_generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Claim,
            request_id,
            Some(pane_id),
            observed_generation,
            0,
            Some(effect_id),
            Vec::new(),
        );
        self.exchange(request)
    }

    pub fn attach(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        request_id: Uuid,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Attach,
            request_id,
            Some(pane_id),
            generation,
            0,
            None,
            Vec::new(),
        );
        self.exchange(request)
    }

    /// Apply one bounded input effect through the guardian's durable
    /// intent/write/disposition transaction. A terminal
    /// `InputKnownNotApplied` rejection proves that zero bytes reached the PTY;
    /// callers may choose a new effect identity, but an exact retry remains
    /// inert and returns the same terminal result.
    pub fn input(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        payload: Vec<u8>,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Input,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            payload,
        );
        self.exchange(request)
    }

    /// Reconcile one prior input effect without retaining or retransmitting its
    /// plaintext. The query contains only the original sequence, byte length,
    /// and authenticated SHA-256 commitment; the exact effect ID and lease
    /// generation prevent a result from being borrowed across mutations.
    /// After an earlier exchange returns an I/O error, use a newly connected
    /// client: the old framed stream may contain a delayed reply and is not a
    /// safe reconciliation channel.
    pub fn query_input_effect(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
        query: GuardianInputEffectQuery,
    ) -> Result<InputEffectState, GuardianClientError> {
        let request = self.request(
            GuardianOperation::QueryInputEffect,
            request_id,
            Some(pane_id),
            generation,
            0,
            Some(effect_id),
            query.encode().to_vec(),
        );
        match self.exchange(request)? {
            GuardianReply::InputEffect {
                effect_id: returned_effect_id,
                state,
            } if returned_effect_id == effect_id => Ok(state),
            _ => Err(GuardianClientError::UnexpectedReply),
        }
    }

    pub fn resize(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        size: PtySize,
    ) -> Result<GuardianReply, GuardianClientError> {
        let payload = GuardianResizePayload::new(size).encode().to_vec();
        let request = self.request(
            GuardianOperation::Resize,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            payload,
        );
        self.exchange(request)
    }

    pub fn terminate(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Signal,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            GuardianSignal::Terminate.encode().to_vec(),
        );
        self.exchange(request)
    }

    pub fn close(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Close,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            Vec::new(),
        );
        self.exchange(request)
    }

    pub fn retire_lease(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::RetireLease,
            request_id,
            Some(pane_id),
            generation,
            sequence,
            Some(effect_id),
            Vec::new(),
        );
        self.exchange(request)
    }

    pub fn census(
        &mut self,
        page: GuardianCensusPageRequest,
    ) -> Result<GuardianReply, GuardianClientError> {
        let request = self.request(
            GuardianOperation::Census,
            Uuid::new_v4(),
            None,
            0,
            0,
            None,
            page.encode().to_vec(),
        );
        self.exchange(request)
    }

    /// Traverse one complete, guardian-bounded census snapshot within one
    /// client I/O deadline. A partial, changing, or overlong traversal is not
    /// accepted as readiness.
    pub fn probe(&mut self) -> Result<GuardianProbeReport, GuardianClientError> {
        let deadline = Instant::now()
            .checked_add(CLIENT_IO_TIMEOUT)
            .ok_or(GuardianClientError::UnexpectedReply)?;
        let result = self.probe_before(deadline);
        let reset_read = self.stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
        let reset_write = self.stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));
        reset_read?;
        reset_write?;
        result
    }

    pub fn guarded_stop(
        &mut self,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<(), GuardianClientError> {
        let request = self.request(
            GuardianOperation::GuardedStop,
            request_id,
            None,
            0,
            0,
            Some(effect_id),
            Vec::new(),
        );
        match self.exchange(request)? {
            GuardianReply::GuardedStopAccepted => Ok(()),
            _ => Err(GuardianClientError::UnexpectedReply),
        }
    }

    fn probe_before(
        &mut self,
        deadline: Instant,
    ) -> Result<GuardianProbeReport, GuardianClientError> {
        let max_pages = GUARDIAN_MAX_PANES
            .div_ceil(usize::from(GUARDIAN_MAX_CENSUS_ENTRIES))
            .max(1);
        let mut snapshot_id = Uuid::nil();
        let mut cursor = 0_u64;
        let mut expected_total = None;
        for _ in 0..max_pages {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    ErrorKind::TimedOut,
                    "guardian probe deadline elapsed",
                )
                .into());
            }
            self.stream.set_read_timeout(Some(remaining))?;
            self.stream.set_write_timeout(Some(remaining))?;
            let page = GuardianCensusPageRequest::new(
                snapshot_id,
                cursor,
                GUARDIAN_MAX_CENSUS_ENTRIES,
                GUARDIAN_MAX_CENSUS_BYTES,
            )?;
            let GuardianReply::CensusPage {
                snapshot_id: returned_snapshot,
                entries,
                next_cursor,
                total_panes,
            } = self.census(page)?
            else {
                return Err(GuardianClientError::UnexpectedReply);
            };
            if expected_total.is_none() {
                expected_total = Some(total_panes);
                snapshot_id = returned_snapshot;
            } else if expected_total != Some(total_panes) || snapshot_id != returned_snapshot {
                return Err(GuardianClientError::UnexpectedReply);
            }
            let entry_count = u64::try_from(entries.len())
                .map_err(|_| GuardianClientError::UnexpectedReply)?;
            let next_observed = cursor
                .checked_add(entry_count)
                .ok_or(GuardianClientError::UnexpectedReply)?;
            match next_cursor {
                Some(next) if next == next_observed && next > cursor => cursor = next,
                None if next_observed == total_panes => {
                    return Ok(GuardianProbeReport {
                        guardian_incarnation: self.guardian_incarnation,
                        pane_count: total_panes,
                    });
                }
                _ => return Err(GuardianClientError::UnexpectedReply),
            }
        }
        Err(GuardianClientError::UnexpectedReply)
    }

    fn request(
        &self,
        operation: GuardianOperation,
        request_id: Uuid,
        pane_id: Option<Uuid>,
        lease_generation: u64,
        lease_sequence: u64,
        effect_id: Option<Uuid>,
        payload: Vec<u8>,
    ) -> GuardianRequestEnvelope {
        GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                operation,
                self.guardian_incarnation,
                self.mux_incarnation,
                request_id,
                pane_id,
                lease_generation,
                lease_sequence,
                effect_id,
                &payload,
            ),
            payload,
        )
    }

    fn exchange(
        &mut self,
        request: GuardianRequestEnvelope,
    ) -> Result<GuardianReply, GuardianClientError> {
        let mut request = OwnedClientRequest::new(request);
        #[cfg(test)]
        request.set_wipe_probe(self.request_wipe_probe.clone());
        let encoded = encode_guardian_request(&self.secret, request.envelope());
        request.zeroize_payload();
        let mut frame = Zeroizing::new(encoded?);
        drop(request);
        let mut authenticated: AuthenticatedGuardianRequest =
            decode_guardian_request(&self.secret, &frame)?;
        retire_authenticated_input_plaintext(&mut authenticated);
        #[cfg(test)]
        if authenticated.header().operation == GuardianOperation::Input {
            if let Some(probe) = self.request_wipe_probe.as_ref() {
                probe.authenticated_input_wipe.store(
                    authenticated.payload().is_empty(),
                    Ordering::SeqCst,
                );
            }
        }
        self.stream.write_all(&frame)?;
        frame.as_mut_slice().zeroize();
        #[cfg(test)]
        if let Some(probe) = self.request_wipe_probe.as_ref() {
            probe
                .encoded_frame_wipe
                .store(frame.iter().all(|byte| *byte == 0), Ordering::SeqCst);
        }
        let response_frame = read_blocking_frame(&mut self.stream)?;
        let response = decode_guardian_response(&self.secret, &response_frame)?;
        let correlated = response.correlate(authenticated.header())?;
        match correlated.header().status {
            GuardianResponseStatus::Success => correlated
                .success_reply(&authenticated)
                .map_err(GuardianClientError::from),
            GuardianResponseStatus::Rejected | GuardianResponseStatus::Terminal => Err(
                GuardianClientError::Rejected(correlated.rejection_code()?),
            ),
            GuardianResponseStatus::Indeterminate => correlated
                .typed_reply(&authenticated)
                .map_err(GuardianClientError::from),
        }
    }
}

/// Input is the uniquely sensitive operation whose reply contract retains its
/// authenticated byte length independently of plaintext. Wipe that decoded
/// copy before the potentially blocking socket write; other operations retain
/// their bounded payload until typed reply validation consumes
/// operation-specific query fields. The encoded frame is independently wiped
/// immediately after the kernel accepts it.
fn retire_authenticated_input_plaintext(authenticated: &mut AuthenticatedGuardianRequest) {
    if authenticated.header().operation == GuardianOperation::Input {
        authenticated.zeroize_payload();
    }
}

/// Create a durable private authentication token, or validate and retain an
/// already safe token. Existing path bytes are never overwritten. A private
/// digest-bound stage readiness record is retained so a host-crash cut before
/// publication can distinguish stable token bytes from an incomplete write.
pub fn provision_guardian_token(
    path: &Path,
) -> Result<ProvisionTokenOutcome, GuardianServiceError> {
    validate_absolute_path(path)?;
    let parent_authority = open_private_parent(path)?;
    provision_guardian_token_in_pinned_parent(path, &parent_authority)
}

/// Provision a private secret through an already pinned parent-directory
/// authority.  Output-key setup uses this entry point so a temporary pathname
/// replacement cannot redirect staging away from the directory whose identity
/// it already authenticated.
pub(crate) fn provision_guardian_token_in_pinned_parent(
    path: &Path,
    parent_authority: &File,
) -> Result<ProvisionTokenOutcome, GuardianServiceError> {
    validate_absolute_path(path)?;
    validate_pinned_private_parent(path, parent_authority)?;
    let active_name = path.file_name().ok_or(
        GuardianServiceError::InvalidConfiguration(
            "guardian token path has no file name",
        ),
    )?;
    match open_private_file_read_at(parent_authority, active_name) {
        Ok(file) => {
            let _ = load_guardian_secret_from_open_file_at(
                parent_authority,
                active_name,
                file,
            )?;
            sync_pinned_private_parent(path, parent_authority)?;
            return Ok(ProvisionTokenOutcome::Existing);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(GuardianServiceError::io("token-absence-check", error)),
    }

    let stage_path = token_stage_path(path)?;
    let commit_path = token_stage_commit_path(&stage_path)?;
    let stage_name = stage_path.file_name().ok_or(
        GuardianServiceError::InvalidConfiguration(
            "guardian token stage path has no file name",
        ),
    )?;
    let commit_name = commit_path.file_name().ok_or(
        GuardianServiceError::InvalidConfiguration(
            "guardian token stage commit path has no file name",
        ),
    )?;
    // Retain the exclusive stage-file lock through publication and active-path
    // validation. A concurrent provisioner therefore cannot mutate the inode
    // after it has become the active token.
    let mut prepared = prepare_token_stage(parent_authority, stage_name, commit_name)?;
    validate_pinned_private_parent(path, parent_authority)?;
    validate_prepared_token_stage_binding(
        parent_authority,
        stage_name,
        commit_name,
        &mut prepared,
    )?;
    match publish_token_stage_noreplace(
        parent_authority,
        stage_name,
        active_name,
    ) {
        Ok(()) => {
            validate_published_token_binding(
                parent_authority,
                stage_name,
                active_name,
                commit_name,
                &mut prepared,
            )?;
            sync_pinned_private_parent(path, parent_authority)?;
            validate_published_token_binding(
                parent_authority,
                stage_name,
                active_name,
                commit_name,
                &mut prepared,
            )?;
            Ok(ProvisionTokenOutcome::Created)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let file = open_private_file_read_at(parent_authority, active_name)
                .map_err(|error| GuardianServiceError::io("token-raced-active-open", error))?;
            let _ = load_guardian_secret_from_open_file_at(
                parent_authority,
                active_name,
                file,
            )?;
            sync_pinned_private_parent(path, parent_authority)?;
            Ok(ProvisionTokenOutcome::Existing)
        }
        Err(error) => Err(GuardianServiceError::io(
            "token-no-replace-publish",
            error,
        )),
    }
}

struct PreparedTokenStage {
    stage: File,
    readiness: File,
    material_digest: [u8; 32],
    readiness_record: [u8; TOKEN_STAGE_COMMIT_BYTES],
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn open_private_file_at(
    parent: &File,
    name: &OsStr,
    create_new: bool,
) -> std::io::Result<File> {
    let mut flags = rustix::fs::OFlags::RDWR
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW;
    if create_new {
        flags |= rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL;
    }
    rustix::fs::openat(
        parent,
        name,
        flags,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn open_private_file_at(
    _parent: &File,
    _name: &OsStr,
    _create_new: bool,
) -> std::io::Result<File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "descriptor-relative guardian token staging is unsupported on this Unix target",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn open_private_file_read_at(parent: &File, name: &OsStr) -> std::io::Result<File> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn open_private_file_read_at(_parent: &File, _name: &OsStr) -> std::io::Result<File> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "descriptor-relative guardian token reads are unsupported on this Unix target",
    ))
}

fn token_stage_path(path: &Path) -> Result<PathBuf, GuardianServiceError> {
    let file_name = path.file_name().ok_or(GuardianServiceError::InvalidConfiguration(
        "guardian token path has no file name",
    ))?;
    let mut stage_name = file_name.to_os_string();
    stage_name.push(".provisioning");
    let stage_path = path.with_file_name(stage_name);
    validate_absolute_path(&stage_path)?;
    if stage_path == path {
        return Err(GuardianServiceError::InvalidConfiguration(
            "guardian token stage path aliases the active path",
        ));
    }
    Ok(stage_path)
}

fn token_stage_commit_path(stage_path: &Path) -> Result<PathBuf, GuardianServiceError> {
    let file_name = stage_path.file_name().ok_or(
        GuardianServiceError::InvalidConfiguration(
            "guardian token stage path has no file name",
        ),
    )?;
    let mut commit_name = file_name.to_os_string();
    commit_name.push(".ready");
    let commit_path = stage_path.with_file_name(commit_name);
    validate_absolute_path(&commit_path)?;
    if commit_path == stage_path {
        return Err(GuardianServiceError::InvalidConfiguration(
            "guardian token stage commit path aliases the token stage",
        ));
    }
    Ok(commit_path)
}

fn prepare_token_stage(
    parent: &File,
    stage_name: &OsStr,
    commit_name: &OsStr,
) -> Result<PreparedTokenStage, GuardianServiceError> {
    let mut file = match open_private_file_at(parent, stage_name, false) {
        Ok(file) => {
            let metadata = file
                .metadata()
                .map_err(|error| GuardianServiceError::io("token-stage-opened-metadata", error))?;
            validate_token_stage_metadata(&metadata)?;
            file
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            match open_private_file_at(parent, stage_name, true) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let file = open_private_file_at(parent, stage_name, false).map_err(|error| {
                        GuardianServiceError::io("token-stage-race-open", error)
                    })?;
                    let metadata = file.metadata().map_err(|error| {
                        GuardianServiceError::io("token-stage-race-metadata", error)
                    })?;
                    validate_token_stage_metadata(&metadata)?;
                    file
                }
                Err(error) => {
                    return Err(GuardianServiceError::io("token-stage-create", error));
                }
            }
        }
        Err(error) => return Err(GuardianServiceError::io("token-stage-open", error)),
    };
    lock_token_stage(&file)
        .map_err(|error| GuardianServiceError::io("token-stage-lock", error))?;
    let before = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-stage-opened-metadata", error))?;
    validate_token_stage_metadata(&before)?;
    require_descriptor_relative_binding(
        parent,
        stage_name,
        &before,
        validate_token_stage_metadata,
        "token-stage-locked-binding-open",
    )?;

    let mut existing = Zeroizing::new([0_u8; GUARDIAN_AUTH_TOKEN_BYTES]);
    let existing_len = usize::try_from(before.len()).map_err(|_| {
        GuardianServiceError::FilesystemSecurity("guardian token stage length is invalid")
    })?;
    file.rewind()
        .map_err(|error| GuardianServiceError::io("token-stage-rewind", error))?;
    file.read_exact(&mut existing[..existing_len])
        .map_err(|error| GuardianServiceError::io("token-stage-read", error))?;
    let complete_nonzero = existing_len == GUARDIAN_AUTH_TOKEN_BYTES
        && existing.iter().any(|byte| *byte != 0);
    let mut commit = open_token_stage_commit(parent, commit_name)?;
    let commit_before = commit
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-stage-commit-metadata", error))?;
    validate_token_stage_commit_metadata(&commit_before)?;
    require_descriptor_relative_binding(
        parent,
        commit_name,
        &commit_before,
        validate_token_stage_commit_metadata,
        "token-stage-commit-binding-open",
    )?;

    let expected_commit = token_stage_commit_record(&existing[..]);
    let mut observed_commit = [0_u8; TOKEN_STAGE_COMMIT_BYTES];
    let commit_len = usize::try_from(commit_before.len()).map_err(|_| {
        GuardianServiceError::FilesystemSecurity(
            "guardian token stage commit length is invalid",
        )
    })?;
    commit
        .rewind()
        .map_err(|error| GuardianServiceError::io("token-stage-commit-rewind", error))?;
    commit
        .read_exact(&mut observed_commit[..commit_len])
        .map_err(|error| GuardianServiceError::io("token-stage-commit-read", error))?;
    let commit_matches = complete_nonzero
        && commit_len == TOKEN_STAGE_COMMIT_BYTES
        && observed_commit == expected_commit;
    if !commit_matches {
        file.set_len(0)
            .map_err(|error| GuardianServiceError::io("token-stage-reset", error))?;
        file.rewind()
            .map_err(|error| GuardianServiceError::io("token-stage-reset-rewind", error))?;
        fill_nonzero_secret(&mut existing)?;
        file.write_all(&existing[..])
            .map_err(|error| GuardianServiceError::io("token-stage-write", error))?;
    }
    file.sync_all()
        .map_err(|error| GuardianServiceError::io("token-stage-sync", error))?;
    let expected_commit = token_stage_commit_record(&existing[..]);
    if !commit_matches {
        commit.set_len(0).map_err(|error| {
            GuardianServiceError::io("token-stage-commit-reset", error)
        })?;
        commit.rewind().map_err(|error| {
            GuardianServiceError::io("token-stage-commit-reset-rewind", error)
        })?;
        commit.write_all(&expected_commit).map_err(|error| {
            GuardianServiceError::io("token-stage-commit-write", error)
        })?;
    }
    commit
        .sync_all()
        .map_err(|error| GuardianServiceError::io("token-stage-commit-sync", error))?;
    let after_open = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-stage-metadata-after", error))?;
    validate_token_metadata(&after_open)?;
    require_same_object(&before, &after_open)?;
    require_descriptor_relative_binding(
        parent,
        stage_name,
        &after_open,
        validate_token_metadata,
        "token-stage-final-binding-open",
    )?;
    let commit_after_open = commit.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-commit-metadata-after", error)
    })?;
    validate_token_stage_commit_ready_metadata(&commit_after_open)?;
    require_same_object(&commit_before, &commit_after_open)?;
    require_descriptor_relative_binding(
        parent,
        commit_name,
        &commit_after_open,
        validate_token_stage_commit_ready_metadata,
        "token-stage-commit-final-binding-open",
    )?;
    sync_private_parent_authority(parent)?;
    let material_digest = Sha256::digest(existing.as_slice()).into();
    Ok(PreparedTokenStage {
        stage: file,
        readiness: commit,
        material_digest,
        readiness_record: expected_commit,
    })
}

fn require_descriptor_relative_binding(
    parent: &File,
    name: &OsStr,
    expected: &Metadata,
    validate: fn(&Metadata) -> Result<(), GuardianServiceError>,
    site: &'static str,
) -> Result<(), GuardianServiceError> {
    let named = open_private_file_read_at(parent, name)
        .map_err(|error| GuardianServiceError::io(site, error))?;
    let named_metadata = named
        .metadata()
        .map_err(|error| GuardianServiceError::io(site, error))?;
    validate(&named_metadata)?;
    require_same_file(expected, &named_metadata)
}

fn token_material_digest(file: &mut File) -> Result<[u8; 32], GuardianServiceError> {
    let before = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-material-metadata-before", error))?;
    validate_token_metadata(&before)?;
    let mut bytes = Zeroizing::new([0_u8; GUARDIAN_AUTH_TOKEN_BYTES]);
    file.rewind()
        .map_err(|error| GuardianServiceError::io("token-material-rewind", error))?;
    file.read_exact(&mut bytes[..])
        .map_err(|error| GuardianServiceError::io("token-material-read", error))?;
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|error| GuardianServiceError::io("token-material-length-recheck", error))?
        != 0
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token material changed length while it was validated",
        ));
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token material cannot be all zero",
        ));
    }
    let after = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-material-metadata-after", error))?;
    validate_token_metadata(&after)?;
    require_same_file(&before, &after)?;
    Ok(Sha256::digest(bytes.as_slice()).into())
}

fn read_token_stage_commit(
    file: &mut File,
) -> Result<[u8; TOKEN_STAGE_COMMIT_BYTES], GuardianServiceError> {
    let before = file.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-commit-read-metadata-before", error)
    })?;
    validate_token_stage_commit_ready_metadata(&before)?;
    let mut record = [0_u8; TOKEN_STAGE_COMMIT_BYTES];
    file.rewind().map_err(|error| {
        GuardianServiceError::io("token-stage-commit-read-rewind", error)
    })?;
    file.read_exact(&mut record).map_err(|error| {
        GuardianServiceError::io("token-stage-commit-read-exact", error)
    })?;
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|error| GuardianServiceError::io("token-stage-commit-read-length", error))?
        != 0
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage readiness changed length while it was validated",
        ));
    }
    let after = file.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-commit-read-metadata-after", error)
    })?;
    validate_token_stage_commit_ready_metadata(&after)?;
    require_same_file(&before, &after)?;
    Ok(record)
}

fn validate_prepared_token_stage_binding(
    parent: &File,
    stage_name: &OsStr,
    commit_name: &OsStr,
    prepared: &mut PreparedTokenStage,
) -> Result<(), GuardianServiceError> {
    let stage_metadata = prepared.stage.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-prepublish-metadata", error)
    })?;
    validate_token_metadata(&stage_metadata)?;
    require_descriptor_relative_binding(
        parent,
        stage_name,
        &stage_metadata,
        validate_token_metadata,
        "token-stage-prepublish-binding-open",
    )?;
    if token_material_digest(&mut prepared.stage)? != prepared.material_digest {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage digest changed before publication",
        ));
    }
    let stage_after = prepared.stage.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-prepublish-metadata-after", error)
    })?;
    require_same_file(&stage_metadata, &stage_after)?;
    require_descriptor_relative_binding(
        parent,
        stage_name,
        &stage_after,
        validate_token_metadata,
        "token-stage-prepublish-final-binding-open",
    )?;

    let readiness_metadata = prepared.readiness.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-readiness-prepublish-metadata", error)
    })?;
    validate_token_stage_commit_ready_metadata(&readiness_metadata)?;
    require_descriptor_relative_binding(
        parent,
        commit_name,
        &readiness_metadata,
        validate_token_stage_commit_ready_metadata,
        "token-stage-readiness-prepublish-binding-open",
    )?;
    if read_token_stage_commit(&mut prepared.readiness)? != prepared.readiness_record {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage readiness digest changed before publication",
        ));
    }
    let readiness_after = prepared.readiness.metadata().map_err(|error| {
        GuardianServiceError::io("token-stage-readiness-prepublish-metadata-after", error)
    })?;
    require_same_file(&readiness_metadata, &readiness_after)?;
    require_descriptor_relative_binding(
        parent,
        commit_name,
        &readiness_after,
        validate_token_stage_commit_ready_metadata,
        "token-stage-readiness-prepublish-final-binding-open",
    )
}

fn validate_published_token_binding(
    parent: &File,
    stage_name: &OsStr,
    active_name: &OsStr,
    commit_name: &OsStr,
    prepared: &mut PreparedTokenStage,
) -> Result<(), GuardianServiceError> {
    match open_private_file_read_at(parent, stage_name) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(GuardianServiceError::io(
                "token-published-stage-absence-check",
                error,
            ));
        }
        Ok(_) => {
            return Err(GuardianServiceError::FilesystemSecurity(
                "guardian token stage still names an object after publication",
            ));
        }
    }

    let stage_metadata = prepared.stage.metadata().map_err(|error| {
        GuardianServiceError::io("token-published-stage-metadata", error)
    })?;
    validate_token_metadata(&stage_metadata)?;
    if token_material_digest(&mut prepared.stage)? != prepared.material_digest {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage digest changed during publication",
        ));
    }

    let mut active = open_private_file_read_at(parent, active_name)
        .map_err(|error| GuardianServiceError::io("token-published-active-open", error))?;
    let active_metadata = active.metadata().map_err(|error| {
        GuardianServiceError::io("token-published-active-metadata", error)
    })?;
    validate_token_metadata(&active_metadata)?;
    require_same_file(&stage_metadata, &active_metadata)?;
    if token_material_digest(&mut active)? != prepared.material_digest {
        return Err(GuardianServiceError::FilesystemSecurity(
            "published guardian token digest does not match its locked stage",
        ));
    }
    require_descriptor_relative_binding(
        parent,
        active_name,
        &active_metadata,
        validate_token_metadata,
        "token-published-active-binding-open",
    )?;

    let readiness_metadata = prepared.readiness.metadata().map_err(|error| {
        GuardianServiceError::io("token-published-readiness-metadata", error)
    })?;
    validate_token_stage_commit_ready_metadata(&readiness_metadata)?;
    require_descriptor_relative_binding(
        parent,
        commit_name,
        &readiness_metadata,
        validate_token_stage_commit_ready_metadata,
        "token-published-readiness-binding-open",
    )?;
    if read_token_stage_commit(&mut prepared.readiness)? != prepared.readiness_record {
        return Err(GuardianServiceError::FilesystemSecurity(
            "published guardian token readiness no longer binds its stage digest",
        ));
    }
    Ok(())
}

fn open_token_stage_commit(
    parent: &File,
    name: &OsStr,
) -> Result<File, GuardianServiceError> {
    let file = match open_private_file_at(parent, name, false) {
        Ok(file) => {
            let opened = file.metadata().map_err(|error| {
                GuardianServiceError::io("token-stage-commit-opened-metadata", error)
            })?;
            validate_token_stage_commit_metadata(&opened)?;
            file
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            match open_private_file_at(parent, name, true) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    open_private_file_at(parent, name, false).map_err(|error| {
                        GuardianServiceError::io("token-stage-commit-race-open", error)
                    })?
                }
                Err(error) => {
                    return Err(GuardianServiceError::io(
                        "token-stage-commit-create",
                        error,
                    ));
                }
            }
        }
        Err(error) => {
            return Err(GuardianServiceError::io("token-stage-commit-open", error));
        }
    };
    lock_token_stage(&file)
        .map_err(|error| GuardianServiceError::io("token-stage-commit-lock", error))?;
    Ok(file)
}

fn token_stage_commit_record(token: &[u8]) -> [u8; TOKEN_STAGE_COMMIT_BYTES] {
    let mut record = [0_u8; TOKEN_STAGE_COMMIT_BYTES];
    record[..TOKEN_STAGE_COMMIT_MAGIC.len()].copy_from_slice(&TOKEN_STAGE_COMMIT_MAGIC);
    let digest: [u8; 32] = Sha256::digest(token).into();
    record[TOKEN_STAGE_COMMIT_MAGIC.len()..].copy_from_slice(&digest);
    record
}

fn validate_token_stage_metadata(metadata: &Metadata) -> Result<(), GuardianServiceError> {
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > u64::try_from(GUARDIAN_AUTH_TOKEN_BYTES).unwrap_or(u64::MAX)
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage must be a current-user, mode-0600, single-link bounded regular file",
        ));
    }
    Ok(())
}

fn validate_token_stage_commit_metadata(
    metadata: &Metadata,
) -> Result<(), GuardianServiceError> {
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > u64::try_from(TOKEN_STAGE_COMMIT_BYTES).unwrap_or(u64::MAX)
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage commit must be a current-user, mode-0600, single-link bounded regular file",
        ));
    }
    Ok(())
}

fn validate_token_stage_commit_ready_metadata(
    metadata: &Metadata,
) -> Result<(), GuardianServiceError> {
    validate_token_stage_commit_metadata(metadata)?;
    if metadata.len() != u64::try_from(TOKEN_STAGE_COMMIT_BYTES).unwrap_or(u64::MAX) {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token stage commit is not complete",
        ));
    }
    Ok(())
}

fn fill_nonzero_secret(
    bytes: &mut [u8; GUARDIAN_AUTH_TOKEN_BYTES],
) -> Result<(), GuardianServiceError> {
    loop {
        getrandom::fill(bytes)?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(());
        }
    }
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn lock_token_stage(file: &std::fs::File) -> std::io::Result<()> {
    rustix::fs::flock(
        file,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn lock_token_stage(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "safe guardian token stage locking is unsupported on this Unix target",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn publish_token_stage_noreplace(
    parent: &std::fs::File,
    stage: &std::ffi::OsStr,
    active: &std::ffi::OsStr,
) -> std::io::Result<()> {
    rustix::fs::renameat_with(
        parent,
        stage,
        parent,
        active,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn publish_token_stage_noreplace(
    _parent: &std::fs::File,
    _stage: &std::ffi::OsStr,
    _active: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "atomic no-replace guardian token publication is unsupported on this Unix target",
    ))
}

fn complete_frame_len(bytes: &[u8]) -> Option<usize> {
    let prefix: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(prefix))
        .ok()?
        .checked_add(4)
}

fn read_blocking_frame(
    stream: &mut BlockingUnixStream,
) -> Result<Zeroizing<Vec<u8>>, GuardianClientError> {
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix)?;
    let body_len = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| GuardianProtocolError::FrameTooLarge)?;
    let total_len = body_len
        .checked_add(prefix.len())
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    if total_len > GUARDIAN_MAX_FRAME_BYTES {
        return Err(GuardianProtocolError::FrameTooLarge.into());
    }
    let mut frame = Zeroizing::new(Vec::new());
    frame
        .try_reserve_exact(total_len)
        .map_err(|_| GuardianProtocolError::FrameTooLarge)?;
    frame.extend_from_slice(&prefix);
    frame.resize(total_len, 0);
    stream.read_exact(&mut frame[prefix.len()..])?;
    Ok(frame)
}

fn validate_absolute_path(path: &Path) -> Result<(), GuardianServiceError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(GuardianServiceError::InvalidConfiguration(
            "guardian paths must be absolute, normalized file paths",
        ));
    }
    Ok(())
}

fn validate_private_parent(path: &Path) -> Result<(), GuardianServiceError> {
    validate_absolute_path(path)?;
    let parent = path.parent().ok_or(GuardianServiceError::FilesystemSecurity(
        "guardian path has no parent directory",
    ))?;
    let mut current = PathBuf::from("/");
    for component in parent.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => current.push(part),
            _ => {
                return Err(GuardianServiceError::FilesystemSecurity(
                    "guardian parent path is not normalized",
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| GuardianServiceError::io("parent-metadata", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(GuardianServiceError::FilesystemSecurity(
                "guardian parent path contains a symlink or non-directory",
            ));
        }
        if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
            return Err(GuardianServiceError::FilesystemSecurity(
                "guardian parent path contains a group-or-other-writable non-sticky directory",
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianServiceError::io("private-parent-metadata", error))?;
    if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian immediate parent must be owned by the current user and owner-only",
        ));
    }
    Ok(())
}

fn open_private_parent(path: &Path) -> Result<std::fs::File, GuardianServiceError> {
    validate_private_parent(path)?;
    let parent = path.parent().ok_or(GuardianServiceError::FilesystemSecurity(
        "guardian path has no parent directory",
    ))?;
    let before = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianServiceError::io("private-parent-metadata-before", error))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|error| GuardianServiceError::io("private-parent-open", error))?;
    let opened = directory
        .metadata()
        .map_err(|error| GuardianServiceError::io("private-parent-opened-metadata", error))?;
    require_same_file(&before, &opened)?;
    let after = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianServiceError::io("private-parent-metadata-after", error))?;
    require_same_file(&opened, &after)?;
    validate_private_parent(path)?;
    Ok(directory)
}

fn validate_pinned_private_parent(
    path: &Path,
    directory: &std::fs::File,
) -> Result<(), GuardianServiceError> {
    validate_private_parent(path)?;
    let parent = path.parent().ok_or(GuardianServiceError::FilesystemSecurity(
        "guardian path has no parent directory",
    ))?;
    let opened = directory
        .metadata()
        .map_err(|error| GuardianServiceError::io("pinned-parent-metadata", error))?;
    let named = std::fs::symlink_metadata(parent)
        .map_err(|error| GuardianServiceError::io("pinned-parent-path-metadata", error))?;
    require_same_file(&opened, &named)
}

fn sync_pinned_private_parent(
    path: &Path,
    directory: &std::fs::File,
) -> Result<(), GuardianServiceError> {
    validate_pinned_private_parent(path, directory)?;
    directory
        .sync_all()
        .map_err(|error| GuardianServiceError::io("pinned-parent-sync", error))?;
    validate_pinned_private_parent(path, directory)
}

fn sync_private_parent_authority(directory: &File) -> Result<(), GuardianServiceError> {
    let before = directory
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-parent-authority-before", error))?;
    if !before.is_dir()
        || before.uid() != geteuid().as_raw()
        || before.mode() & 0o077 != 0
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian pinned parent authority is not a current-user owner-only directory",
        ));
    }
    directory
        .sync_all()
        .map_err(|error| GuardianServiceError::io("token-parent-authority-sync", error))?;
    let after = directory
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-parent-authority-after", error))?;
    require_same_file(&before, &after)
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn require_absent_at(parent: &File, name: &OsStr) -> Result<(), GuardianServiceError> {
    match rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if std::io::Error::from(error).kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(GuardianServiceError::io(
            "socket-absence-check-at",
            std::io::Error::from(error),
        )),
        Ok(_) => Err(GuardianServiceError::FilesystemSecurity(
            "guardian socket path already exists; stale sockets are not removed automatically",
        )),
    }
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn require_absent_at(_parent: &File, _name: &OsStr) -> Result<(), GuardianServiceError> {
    Err(GuardianServiceError::FilesystemSecurity(
        "descriptor-relative guardian socket validation is unsupported on this Unix target",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn chmod_socket_at(parent: &File, name: &OsStr) -> Result<(), GuardianServiceError> {
    rustix::fs::chmodat(
        parent,
        name,
        rustix::fs::Mode::from_raw_mode(0o600),
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|error| GuardianServiceError::io("socket-chmod-at", std::io::Error::from(error)))
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn chmod_socket_at(_parent: &File, _name: &OsStr) -> Result<(), GuardianServiceError> {
    Err(GuardianServiceError::FilesystemSecurity(
        "descriptor-relative guardian socket permission changes are unsupported on this Unix target",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn socket_path_identity_at(
    parent: &File,
    name: &OsStr,
) -> Result<SocketPathIdentity, GuardianServiceError> {
    let metadata = rustix::fs::statat(
        parent,
        name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|error| {
        GuardianServiceError::io("socket-metadata-at", std::io::Error::from(error))
    })?;
    let mode = u32::try_from(metadata.st_mode).map_err(|_| {
        GuardianServiceError::FilesystemSecurity("guardian socket mode is not representable")
    })?;
    let owner = u32::try_from(metadata.st_uid).map_err(|_| {
        GuardianServiceError::FilesystemSecurity("guardian socket owner is not representable")
    })?;
    let links = u64::try_from(metadata.st_nlink).map_err(|_| {
        GuardianServiceError::FilesystemSecurity("guardian socket link count is not representable")
    })?;
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::Socket
        || owner != geteuid().as_raw()
        || mode & 0o777 != 0o600
        || links != 1
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "bound guardian socket identity, owner, mode, or link count is invalid",
        ));
    }
    Ok(SocketPathIdentity {
        device: u64::try_from(metadata.st_dev).map_err(|_| {
            GuardianServiceError::FilesystemSecurity(
                "guardian socket device identity is not representable",
            )
        })?,
        inode: u64::try_from(metadata.st_ino).map_err(|_| {
            GuardianServiceError::FilesystemSecurity(
                "guardian socket inode identity is not representable",
            )
        })?,
        mode,
        owner,
        links,
    })
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn socket_path_identity_at(
    _parent: &File,
    _name: &OsStr,
) -> Result<SocketPathIdentity, GuardianServiceError> {
    Err(GuardianServiceError::FilesystemSecurity(
        "descriptor-relative guardian socket identity is unsupported on this Unix target",
    ))
}

fn prove_socket_path_routes_to_listener(
    listener: &mut UnixListener,
    socket_path: &Path,
) -> Result<(), GuardianServiceError> {
    let mut challenge = Zeroizing::new([0_u8; GUARDIAN_AUTH_TOKEN_BYTES]);
    getrandom::fill(&mut challenge[..])?;
    let mut probe = BlockingUnixStream::connect(socket_path)
        .map_err(|error| GuardianServiceError::io("socket-listener-proof-connect", error))?;
    probe
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| GuardianServiceError::io("socket-listener-proof-timeout", error))?;
    let (mut accepted, _) = loop {
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(GuardianServiceError::io(
                    "socket-listener-proof-accept",
                    error,
                ));
            }
        }
    };
    accepted
        .write_all(&challenge[..])
        .map_err(|error| GuardianServiceError::io("socket-listener-proof-write", error))?;
    let mut observed = Zeroizing::new([0_u8; GUARDIAN_AUTH_TOKEN_BYTES]);
    probe
        .read_exact(&mut observed[..])
        .map_err(|error| GuardianServiceError::io("socket-listener-proof-read", error))?;
    if observed.as_slice() != challenge.as_slice() {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian socket path did not route to the newly bound listener",
        ));
    }
    Ok(())
}

fn validate_bound_socket(path: &Path) -> Result<(), GuardianServiceError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| GuardianServiceError::io("bound-socket-metadata", error))?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "bound guardian socket identity, owner, mode, or link count is invalid",
        ));
    }
    Ok(())
}

fn validate_existing_socket(path: &Path) -> Result<(), GuardianServiceError> {
    validate_private_parent(path)?;
    validate_bound_socket(path)
}

fn load_guardian_secret(path: &Path) -> Result<GuardianSecret, GuardianServiceError> {
    let parent = open_private_parent(path)?;
    let name = path.file_name().ok_or(
        GuardianServiceError::InvalidConfiguration(
            "guardian token path has no file name",
        ),
    )?;
    let file = open_private_file_read_at(&parent, name)
        .map_err(|error| GuardianServiceError::io("token-open", error))?;
    let secret = load_guardian_secret_from_open_file_at(&parent, name, file)?;
    validate_pinned_private_parent(path, &parent)?;
    Ok(secret)
}

fn load_guardian_secret_from_open_file_at(
    parent: &File,
    name: &OsStr,
    mut file: File,
) -> Result<GuardianSecret, GuardianServiceError> {
    let opened = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-opened-metadata", error))?;
    validate_token_metadata(&opened)?;

    let mut bytes = Zeroizing::new([0_u8; GUARDIAN_AUTH_TOKEN_BYTES]);
    file.rewind()
        .map_err(|error| GuardianServiceError::io("token-rewind", error))?;
    file.read_exact(&mut bytes[..])
        .map_err(|error| GuardianServiceError::io("token-read", error))?;
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|error| GuardianServiceError::io("token-length-recheck", error))?
        != 0
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token is not exactly 32 bytes",
        ));
    }

    let after_open = file
        .metadata()
        .map_err(|error| GuardianServiceError::io("token-opened-metadata-after", error))?;
    validate_token_metadata(&after_open)?;
    require_same_file(&opened, &after_open)?;
    require_descriptor_relative_binding(
        parent,
        name,
        &after_open,
        validate_token_metadata,
        "token-final-binding-open",
    )?;
    GuardianSecret::from_bytes(*bytes).map_err(GuardianServiceError::from)
}

fn validate_token_metadata(metadata: &Metadata) -> Result<(), GuardianServiceError> {
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != u64::try_from(GUARDIAN_AUTH_TOKEN_BYTES).unwrap_or(u64::MAX)
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token must be a current-user, mode-0600, single-link 32-byte regular file",
        ));
    }
    Ok(())
}

fn require_same_file(left: &Metadata, right: &Metadata) -> Result<(), GuardianServiceError> {
    if left.dev() != right.dev()
        || left.ino() != right.ino()
        || left.len() != right.len()
        || left.mode() != right.mode()
        || left.uid() != right.uid()
        || left.nlink() != right.nlink()
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian token identity changed while it was being opened",
        ));
    }
    Ok(())
}

fn require_same_object(left: &Metadata, right: &Metadata) -> Result<(), GuardianServiceError> {
    if left.dev() != right.dev()
        || left.ino() != right.ino()
        || left.mode() != right.mode()
        || left.uid() != right.uid()
        || left.nlink() != right.nlink()
    {
        return Err(GuardianServiceError::FilesystemSecurity(
            "guardian filesystem object identity changed during mutation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;
    use std::os::unix::fs::symlink;

    fn authority(connection: usize, request: u128, effect: u128) -> GuardedStopAuthority {
        GuardedStopAuthority {
            connection: Token(connection),
            request_id: Uuid::from_u128(request),
            effect_id: Uuid::from_u128(effect),
        }
    }

    #[test]
    fn completion_token_precedes_the_monotonic_pty_token_range() {
        let max_connections = 64_usize;
        let (output_completion_token, first_pty_token) =
            partition_endpoint_tokens(max_connections).expect("bounded token partition");

        assert_eq!(LISTENER_TOKEN, Token(0));
        assert_eq!(output_completion_token, Token(65));
        assert_eq!(first_pty_token, 66);
        assert!(output_completion_token.0 > max_connections);
        assert!(first_pty_token > output_completion_token.0);
        for offset in 0..100_000_usize {
            assert_ne!(
                first_pty_token.checked_add(offset),
                Some(output_completion_token.0),
                "monotonic PTY churn must never enter the fixed completion token"
            );
        }
        assert_eq!(partition_endpoint_tokens(usize::MAX), None);
    }

    #[test]
    fn delayed_input_completion_cannot_route_to_a_recycled_connection_token() {
        let original = GuardianInputRoute::new(
            Token(7),
            11,
            Uuid::from_u128(12),
            Uuid::from_u128(13),
        )
        .unwrap();
        let recycled = GuardianInputRoute::new(
            Token(7),
            12,
            Uuid::from_u128(12),
            Uuid::from_u128(13),
        )
        .unwrap();
        let aliased_request = GuardianInputRoute::new(
            Token(7),
            11,
            Uuid::from_u128(14),
            Uuid::from_u128(13),
        )
        .unwrap();
        let aliased_effect = GuardianInputRoute::new(
            Token(7),
            11,
            Uuid::from_u128(12),
            Uuid::from_u128(15),
        )
        .unwrap();

        assert!(pending_input_route_matches(11, Some(original), original));
        assert!(!pending_input_route_matches(12, Some(recycled), original));
        assert!(!pending_input_route_matches(
            11,
            Some(aliased_request),
            original,
        ));
        assert!(!pending_input_route_matches(
            11,
            Some(aliased_effect),
            original,
        ));
        assert!(!pending_input_route_matches(11, None, original));
        assert!(GuardianInputRoute::new(
            Token(7),
            0,
            Uuid::from_u128(12),
            Uuid::from_u128(13),
        )
        .is_none());
    }

    #[test]
    fn lifecycle_epochs_order_delayed_hello_and_exact_recycled_membership() {
        let mux_incarnation = Uuid::from_u128(201);
        let other_mux = Uuid::from_u128(202);
        let delayed_lower_generation = ConnectionIdentity {
            token: Token(1),
            generation: 2,
        };
        let initially_authenticated = ConnectionIdentity {
            token: Token(2),
            generation: 9,
        };
        let mut tracker = MuxConnectionTracker::new(8).unwrap();
        tracker.observe_accept(delayed_lower_generation).unwrap();
        tracker.observe_accept(initially_authenticated).unwrap();
        tracker
            .observe_authenticated_hello(initially_authenticated, mux_incarnation)
            .unwrap();
        tracker
            .observe_disconnect(initially_authenticated, Some(mux_incarnation))
            .unwrap();
        let first_disconnect = tracker.pending_retirements[0];

        tracker
            .observe_authenticated_hello(delayed_lower_generation, mux_incarnation)
            .expect("processed Hello order, not accept generation, cancels retirement");
        assert!(tracker.pending_retirements.is_empty());
        assert!(tracker.has_authenticated_membership(mux_incarnation));

        tracker
            .observe_disconnect(delayed_lower_generation, Some(mux_incarnation))
            .unwrap();
        let later_disconnect = tracker.pending_retirements[0];
        assert!(
            later_disconnect.disconnect_observation_epoch
                > first_disconnect.disconnect_observation_epoch
        );

        let unrelated = ConnectionIdentity {
            token: Token(3),
            generation: 3,
        };
        tracker.observe_accept(unrelated).unwrap();
        tracker
            .observe_authenticated_hello(unrelated, other_mux)
            .unwrap();
        assert_eq!(
            tracker.next_replayable_retirement().unwrap(),
            Some(later_disconnect),
            "another mux's exact live membership cannot block this retirement"
        );

        let stale = ConnectionIdentity {
            token: Token(4),
            generation: 40,
        };
        let recycled = ConnectionIdentity {
            token: Token(4),
            generation: 41,
        };
        tracker.observe_accept(stale).unwrap();
        tracker.observe_disconnect(stale, None).unwrap();
        tracker.observe_accept(recycled).unwrap();
        assert_eq!(
            tracker.observe_authenticated_hello(stale, mux_incarnation),
            Err(MuxConnectionTrackingError::StaleConnection)
        );
        assert_eq!(
            tracker.observe_disconnect(stale, None),
            Err(MuxConnectionTrackingError::StaleConnection)
        );
        assert_eq!(tracker.pending_retirements, [later_disconnect]);
        tracker
            .observe_authenticated_hello(recycled, mux_incarnation)
            .unwrap();
        assert!(tracker.pending_retirements.is_empty());
        tracker
            .observe_disconnect(recycled, Some(mux_incarnation))
            .unwrap();
        assert!(
            tracker.pending_retirements[0].disconnect_observation_epoch
                > later_disconnect.disconnect_observation_epoch
        );
    }

    #[test]
    fn replay_fails_closed_if_exact_authenticated_membership_is_active() {
        let mux_incarnation = Uuid::from_u128(211);
        let identity = ConnectionIdentity {
            token: Token(5),
            generation: 7,
        };
        let mut tracker = MuxConnectionTracker::new(4).unwrap();
        tracker.observe_accept(identity).unwrap();
        tracker
            .observe_authenticated_hello(identity, mux_incarnation)
            .unwrap();
        tracker
            .pending_retirements
            .push(PendingMuxRetirementObservation {
                mux_incarnation,
                disconnect_observation_epoch: 1,
            });

        assert_eq!(
            tracker.next_replayable_retirement(),
            Err(MuxConnectionTrackingError::ActiveMembershipAtReplay)
        );
        assert!(tracker.has_authenticated_membership(mux_incarnation));
        assert_eq!(tracker.pending_retirements.len(), 1);
    }

    #[test]
    fn guardian_service_source_wires_exact_lifecycle_tracker_at_every_boundary() {
        let source = include_str!("transport.rs");
        let service_source = source
            .split("/// Blocking client used by mux integration")
            .next()
            .expect("service implementation precedes client implementation");
        let poll_once_source = service_source
            .split("    pub fn poll_once")
            .nth(1)
            .and_then(|source| source.split("    fn accept_connections").next())
            .expect("poll_once production body is present");
        let handle_input_completions = poll_once_source
            .find("self.handle_input_completions();")
            .expect("input authority restoration is wired into poll_once");
        let retirement_replay = poll_once_source
            .find("self.replay_deferred_mux_retirements();")
            .expect("retirement replay is wired into poll_once");
        let finish_connection_source = service_source
            .split("    fn finish_connection")
            .nth(1)
            .and_then(|source| {
                source.split("    /// Replay only a readiness-loop-owned").next()
            })
            .expect("finish_connection production body is present");

        assert!(service_source.contains("self.mux_connections.observe_accept(identity)"));
        assert!(service_source.contains(".observe_authenticated_hello(\n                            connection.identity(token),\n                            mux_incarnation,"));
        assert!(
            service_source
                .contains(".observe_disconnect(identity, connection.mux_incarnation)")
        );
        assert!(retirement_replay > handle_input_completions);
        assert_eq!(
            poll_once_source
                .matches("self.replay_deferred_mux_retirements();")
                .count(),
            1
        );
        assert!(!finish_connection_source.contains("replay_deferred_mux_retirements"));
        assert!(service_source.contains(".next_replayable_retirement()"));
        assert!(service_source.contains(".retire_disconnected_mux(retirement.mux_incarnation)"));
        assert!(!service_source.contains("active_mux_connections"));
        assert!(!service_source.contains("observe_connected_mux"));
    }

    #[test]
    fn exchange_explicitly_wipes_moved_request_before_encode_error_propagates() {
        let (client_stream, _peer_stream) = BlockingUnixStream::pair().unwrap();
        let secret = GuardianSecret::from_bytes([0x5a; GUARDIAN_AUTH_TOKEN_BYTES]).unwrap();
        let wipe_probe = Arc::new(ClientRequestWipeProbe::default());
        let mut client = GuardianClient {
            stream: client_stream,
            secret,
            mux_incarnation: Uuid::from_u128(21),
            guardian_incarnation: Uuid::from_u128(22),
            request_wipe_probe: Some(Arc::clone(&wipe_probe)),
        };
        let header_commitment_source = [0x19; 11];
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Input,
                Uuid::from_u128(22),
                Uuid::from_u128(21),
                Uuid::from_u128(23),
                Some(Uuid::from_u128(24)),
                3,
                7,
                Some(Uuid::from_u128(25)),
                &header_commitment_source,
            ),
            vec![0xa5; 29],
        );

        assert!(matches!(
            client.exchange(request),
            Err(GuardianClientError::Protocol(
                GuardianProtocolError::PayloadDigestMismatch
            ))
        ));
        assert!(wipe_probe.explicit_wipe.load(Ordering::SeqCst));
        assert!(wipe_probe.drop_wipe.load(Ordering::SeqCst));
        assert!(!wipe_probe.authenticated_input_wipe.load(Ordering::SeqCst));
        assert!(!wipe_probe.encoded_frame_wipe.load(Ordering::SeqCst));
    }

    #[test]
    fn guardian_client_source_wires_plaintext_retirement_before_blocking_boundaries() {
        let source = include_str!("transport.rs");
        let exchange_source = source
            .split("    fn exchange(")
            .nth(1)
            .and_then(|source| source.split("/// Input is the uniquely sensitive").next())
            .expect("GuardianClient::exchange production body is present");
        let boundary = |needle: &str| {
            exchange_source
                .find(needle)
                .unwrap_or_else(|| panic!("missing production exchange boundary: {needle}"))
        };
        let ordered_boundaries = [
            boundary("let encoded = encode_guardian_request"),
            boundary("request.zeroize_payload();"),
            boundary("Zeroizing::new(encoded?)"),
            boundary("drop(request);"),
            boundary("decode_guardian_request"),
            boundary("retire_authenticated_input_plaintext(&mut authenticated);"),
            boundary("self.stream.write_all(&frame)?;"),
            boundary("frame.as_mut_slice().zeroize();"),
            boundary("read_blocking_frame(&mut self.stream)?"),
        ];

        assert!(
            ordered_boundaries.windows(2).all(|pair| pair[0] < pair[1]),
            "owned request, decoded Input, and encoded frame must die before their next blocking boundary"
        );
    }

    #[test]
    fn production_input_exchange_validates_reply_after_both_plaintext_copies_die() {
        let (client_stream, mut server_stream) = BlockingUnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        client_stream
            .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        server_stream
            .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        server_stream
            .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        let secret_bytes = [0x5a; GUARDIAN_AUTH_TOKEN_BYTES];
        let guardian_incarnation = Uuid::from_u128(31);
        let mux_incarnation = Uuid::from_u128(32);
        let pane_id = Uuid::from_u128(33);
        let request_id = Uuid::from_u128(34);
        let effect_id = Uuid::from_u128(35);
        let wipe_probe = Arc::new(ClientRequestWipeProbe::default());
        let server_wipe_probe = Arc::clone(&wipe_probe);
        let server = std::thread::spawn(move || {
            let secret = GuardianSecret::from_bytes(secret_bytes).unwrap();
            let frame = read_blocking_frame(&mut server_stream).unwrap();
            let request = decode_guardian_request(&secret, &frame).unwrap();
            assert_eq!(request.header().operation, GuardianOperation::Input);
            assert_eq!(request.authenticated_payload_bytes(), 23);
            let wipe_deadline = Instant::now() + Duration::from_secs(3);
            while (!server_wipe_probe
                .authenticated_input_wipe
                .load(Ordering::SeqCst)
                || !server_wipe_probe
                    .encoded_frame_wipe
                    .load(Ordering::SeqCst))
                && Instant::now() < wipe_deadline
            {
                std::thread::yield_now();
            }
            assert!(
                server_wipe_probe
                    .authenticated_input_wipe
                    .load(Ordering::SeqCst)
            );
            assert!(server_wipe_probe.encoded_frame_wipe.load(Ordering::SeqCst));
            let response = GuardianResponseEnvelope::reply(
                &request,
                &GuardianReply::InputReceipt {
                    pane_id,
                    generation: 4,
                    sequence: 9,
                    effect_id,
                    state: InputEffectState::DurableFull,
                },
            )
            .unwrap();
            let frame = encode_guardian_response(&secret, &response).unwrap();
            server_stream.write_all(&frame).unwrap();
        });
        let mut client = GuardianClient {
            stream: client_stream,
            secret: GuardianSecret::from_bytes(secret_bytes).unwrap(),
            mux_incarnation,
            guardian_incarnation,
            request_wipe_probe: Some(Arc::clone(&wipe_probe)),
        };

        assert!(matches!(
            client
                .input(pane_id, 4, 9, request_id, effect_id, vec![0x6d; 23])
                .unwrap(),
            GuardianReply::InputReceipt {
                state: InputEffectState::DurableFull,
                ..
            }
        ));
        assert!(wipe_probe.explicit_wipe.load(Ordering::SeqCst));
        assert!(wipe_probe.drop_wipe.load(Ordering::SeqCst));
        assert!(wipe_probe.authenticated_input_wipe.load(Ordering::SeqCst));
        assert!(wipe_probe.encoded_frame_wipe.load(Ordering::SeqCst));
        server.join().expect("input test server exits cleanly");
    }

    #[test]
    fn query_payload_survives_until_its_typed_reply_is_validated() {
        let secret =
            GuardianSecret::from_bytes([0x5a; GUARDIAN_AUTH_TOKEN_BYTES]).unwrap();
        let effect_id = Uuid::from_u128(31);
        let query = GuardianInputEffectQuery::new(
            8,
            9,
            Sha256::digest(b"forgotten-input").into(),
        )
        .unwrap();
        let encoded_query = query.encode();
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::QueryInputEffect,
                Uuid::from_u128(32),
                Uuid::from_u128(33),
                Uuid::from_u128(34),
                Some(Uuid::from_u128(35)),
                4,
                0,
                Some(effect_id),
                &encoded_query,
            ),
            encoded_query.to_vec(),
        );
        let frame = encode_guardian_request(&secret, &request).unwrap();
        let mut authenticated = decode_guardian_request(&secret, &frame).unwrap();

        retire_authenticated_input_plaintext(&mut authenticated);
        assert_eq!(authenticated.payload(), encoded_query.as_slice());
        GuardianResponseEnvelope::reply(
            &authenticated,
            &GuardianReply::InputEffect {
                effect_id,
                state: InputEffectState::DurablePrefix { applied_bytes: 5 },
            },
        )
        .expect("query plaintext remains until operation-specific validation");
    }

    #[test]
    fn client_query_input_effect_round_trips_typed_plaintext_free_state() {
        let (client_stream, mut server_stream) = BlockingUnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        client_stream
            .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        server_stream
            .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();
        server_stream
            .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
            .unwrap();

        let secret_bytes = [0x5a; GUARDIAN_AUTH_TOKEN_BYTES];
        let guardian_incarnation = Uuid::from_u128(41);
        let mux_incarnation = Uuid::from_u128(42);
        let pane_id = Uuid::from_u128(43);
        let request_id = Uuid::from_u128(44);
        let effect_id = Uuid::from_u128(45);
        let query = GuardianInputEffectQuery::new(
            12,
            16,
            Sha256::digest(b"input-no-longer-retained").into(),
        )
        .unwrap();
        let expected_state = InputEffectState::DurablePrefix { applied_bytes: 6 };
        let server = std::thread::spawn(move || {
            let secret = GuardianSecret::from_bytes(secret_bytes).unwrap();
            let frame = read_blocking_frame(&mut server_stream).unwrap();
            let request = decode_guardian_request(&secret, &frame).unwrap();
            assert_eq!(request.header().operation, GuardianOperation::QueryInputEffect);
            assert_eq!(request.header().guardian_incarnation, guardian_incarnation);
            assert_eq!(request.header().mux_incarnation, mux_incarnation);
            assert_eq!(request.header().request_id, request_id);
            assert_eq!(request.header().pane_id, Some(pane_id));
            assert_eq!(request.header().lease_generation, 5);
            assert_eq!(request.header().lease_sequence, 0);
            assert_eq!(request.header().effect_id, Some(effect_id));
            assert_eq!(request.payload(), query.encode().as_slice());

            let response = GuardianResponseEnvelope::reply(
                &request,
                &GuardianReply::InputEffect {
                    effect_id,
                    state: expected_state,
                },
            )
            .unwrap();
            let frame = encode_guardian_response(&secret, &response).unwrap();
            server_stream.write_all(&frame).unwrap();
        });
        let mut client = GuardianClient {
            stream: client_stream,
            secret: GuardianSecret::from_bytes(secret_bytes).unwrap(),
            mux_incarnation,
            guardian_incarnation,
            request_wipe_probe: None,
        };

        assert_eq!(
            client
                .query_input_effect(pane_id, 5, request_id, effect_id, query)
                .unwrap(),
            expected_state
        );
        server.join().expect("query test server exits cleanly");
    }

    #[test]
    fn guarded_stop_refuses_nonempty_without_poisoning_service() {
        let mut lifecycle = GuardianLifecycle::Running;
        let stop = authority(7, 8, 9);
        assert_eq!(
            lifecycle.begin_guarded_stop(stop, 1),
            Err(GuardianRejectionCode::OwnedPanesPresent)
        );
        assert_eq!(lifecycle, GuardianLifecycle::Running);
        assert_eq!(lifecycle.request_fence(), Ok(()));
        assert_eq!(lifecycle.begin_guarded_stop(stop, 0), Ok(()));
    }

    #[test]
    fn guarded_stop_drains_fences_and_waits_for_exact_response_flush() {
        let stop = authority(7, 8, 9);
        let other = authority(10, 11, 12);
        let mut lifecycle = GuardianLifecycle::Running;
        lifecycle.begin_guarded_stop(stop, 0).unwrap();

        assert_eq!(lifecycle, GuardianLifecycle::Draining(stop));
        assert_eq!(
            lifecycle.request_fence(),
            Err(GuardianRejectionCode::InvalidRequest)
        );
        assert_eq!(
            lifecycle.begin_guarded_stop(other, 0),
            Err(GuardianRejectionCode::InvalidRequest)
        );
        assert!(!lifecycle.response_flushed(other));
        assert_eq!(lifecycle, GuardianLifecycle::Draining(stop));
        lifecycle.authority_disconnected(other.connection);
        assert_eq!(lifecycle, GuardianLifecycle::Draining(stop));

        assert!(lifecycle.response_flushed(stop));
        assert_eq!(lifecycle, GuardianLifecycle::ExitReady);
    }

    #[test]
    fn guarded_stop_disconnect_before_flush_cancels_drain() {
        let stop = authority(7, 8, 9);
        let mut lifecycle = GuardianLifecycle::Running;
        lifecycle.begin_guarded_stop(stop, 0).unwrap();
        lifecycle.authority_disconnected(stop.connection);
        assert_eq!(lifecycle, GuardianLifecycle::Running);
        assert!(!lifecycle.response_flushed(stop));
        assert_eq!(lifecycle.request_fence(), Ok(()));
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn socket_authority_detects_leaf_replacement_after_listener_proof() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-socket-authority-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket_path = directory.join("guardian.sock");
        let parent = open_private_parent(&socket_path).unwrap();
        let name = socket_path.file_name().unwrap();
        require_absent_at(&parent, name).unwrap();
        let mut listener = UnixListener::bind(&socket_path).unwrap();
        chmod_socket_at(&parent, name).unwrap();
        prove_socket_path_routes_to_listener(&mut listener, &socket_path).unwrap();
        let authority = SocketPathAuthority {
            parent,
            socket_path: socket_path.clone(),
            leaf_name: name.to_os_string(),
            identity: socket_path_identity_at(
                &open_private_parent(&socket_path).unwrap(),
                name,
            )
            .unwrap(),
        };
        authority.validate().unwrap();

        let retained_socket = directory.join(format!("guardian-retained-{}.sock", Uuid::new_v4()));
        std::fs::rename(&socket_path, &retained_socket).unwrap();
        let _replacement_listener = UnixListener::bind(&socket_path).unwrap();
        chmod_socket_at(&authority.parent, name).unwrap();

        assert!(authority.validate().is_err());
        assert!(retained_socket.exists());
    }

    #[test]
    fn guardian_paths_reject_writable_non_sticky_ancestor_components() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-unsafe-ancestor-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shared = directory.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let private = shared.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(validate_private_parent(&private.join("guardian.sock")).is_err());
    }

    #[test]
    fn provision_token_is_private_durable_idempotent_and_no_follow() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let token_path = directory.join("guardian.token");

        assert_eq!(
            provision_guardian_token(&token_path).unwrap(),
            ProvisionTokenOutcome::Created
        );
        let first_bytes = std::fs::read(&token_path).unwrap();
        let metadata = std::fs::symlink_metadata(&token_path).unwrap();
        assert_eq!(first_bytes.len(), GUARDIAN_AUTH_TOKEN_BYTES);
        assert!(first_bytes.iter().any(|byte| *byte != 0));
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);

        assert_eq!(
            provision_guardian_token(&token_path).unwrap(),
            ProvisionTokenOutcome::Existing
        );
        assert_eq!(std::fs::read(&token_path).unwrap(), first_bytes);

        let link_path = directory.join("guardian-link.token");
        symlink(&token_path, &link_path).unwrap();
        assert!(matches!(
            provision_guardian_token(&link_path),
            Err(GuardianServiceError::FilesystemSecurity(_))
        ));
        assert!(std::fs::symlink_metadata(&link_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&token_path).unwrap(), first_bytes);

        let invalid_path = directory.join("invalid-existing.token");
        let invalid_bytes = [0_u8; GUARDIAN_AUTH_TOKEN_BYTES];
        let mut invalid = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&invalid_path)
            .unwrap();
        invalid.write_all(&invalid_bytes).unwrap();
        invalid.sync_all().unwrap();
        assert!(provision_guardian_token(&invalid_path).is_err());
        assert_eq!(std::fs::read(&invalid_path).unwrap(), invalid_bytes);
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn token_publication_is_pinned_and_parent_path_replacement_fails_validation() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-pinned-parent-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let active_path = directory.join("guardian.token");
        let stage_path = token_stage_path(&active_path).unwrap();
        let mut stage = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stage_path)
            .unwrap();
        stage
            .write_all(&[0x5a; GUARDIAN_AUTH_TOKEN_BYTES])
            .unwrap();
        stage.sync_all().unwrap();
        drop(stage);

        let pinned = open_private_parent(&active_path).unwrap();
        let retained = directory.with_file_name(format!(
            "frankenterm-guardian-token-retained-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(validate_pinned_private_parent(&active_path, &pinned).is_err());
        publish_token_stage_noreplace(
            &pinned,
            stage_path.file_name().unwrap(),
            active_path.file_name().unwrap(),
        )
        .unwrap();
        assert!(!active_path.exists());
        assert_eq!(
            std::fs::read(retained.join("guardian.token")).unwrap(),
            [0x5a; GUARDIAN_AUTH_TOKEN_BYTES]
        );
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn token_stage_parent_aba_restoration_cannot_publish_a_different_inode() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-parent-aba-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let active_path = directory.join("guardian.token");
        let stage_path = token_stage_path(&active_path).unwrap();
        let commit_path = token_stage_commit_path(&stage_path).unwrap();
        let original_bytes = [0x31_u8; GUARDIAN_AUTH_TOKEN_BYTES];
        let replacement_bytes = [0x72_u8; GUARDIAN_AUTH_TOKEN_BYTES];

        let mut original_stage = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stage_path)
            .unwrap();
        original_stage.write_all(&original_bytes).unwrap();
        original_stage.sync_all().unwrap();
        let mut original_commit = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&commit_path)
            .unwrap();
        original_commit
            .write_all(&token_stage_commit_record(&original_bytes))
            .unwrap();
        original_commit.sync_all().unwrap();
        drop(original_stage);
        drop(original_commit);

        let pinned = open_private_parent(&active_path).unwrap();
        let retained_original = directory.with_file_name(format!(
            "frankenterm-guardian-token-parent-aba-original-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained_original).unwrap();
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut replacement_stage = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stage_path)
            .unwrap();
        replacement_stage.write_all(&replacement_bytes).unwrap();
        replacement_stage.sync_all().unwrap();
        let mut replacement_commit = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&commit_path)
            .unwrap();
        replacement_commit
            .write_all(&token_stage_commit_record(&replacement_bytes))
            .unwrap();
        replacement_commit.sync_all().unwrap();
        drop(replacement_stage);
        drop(replacement_commit);

        let stage_name = stage_path.file_name().unwrap();
        let commit_name = commit_path.file_name().unwrap();
        let mut prepared = prepare_token_stage(&pinned, stage_name, commit_name).unwrap();
        let original_digest: [u8; 32] = Sha256::digest(original_bytes).into();
        assert_eq!(prepared.material_digest, original_digest);

        let retained_replacement = directory.with_file_name(format!(
            "frankenterm-guardian-token-parent-aba-replacement-{}",
            Uuid::new_v4()
        ));
        std::fs::rename(&directory, &retained_replacement).unwrap();
        std::fs::rename(&retained_original, &directory).unwrap();
        validate_pinned_private_parent(&active_path, &pinned).unwrap();
        validate_prepared_token_stage_binding(
            &pinned,
            stage_name,
            commit_name,
            &mut prepared,
        )
        .unwrap();
        publish_token_stage_noreplace(
            &pinned,
            stage_name,
            active_path.file_name().unwrap(),
        )
        .unwrap();
        validate_published_token_binding(
            &pinned,
            stage_name,
            active_path.file_name().unwrap(),
            commit_name,
            &mut prepared,
        )
        .unwrap();

        assert_eq!(std::fs::read(&active_path).unwrap(), original_bytes);
        assert_eq!(
            std::fs::read(retained_replacement.join(stage_name)).unwrap(),
            replacement_bytes
        );
    }

    #[cfg(any(
        target_os = "android",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    #[test]
    fn token_stage_leaf_replacement_is_rejected_before_publication() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-stage-swap-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let active_path = directory.join("guardian.token");
        let stage_path = token_stage_path(&active_path).unwrap();
        let commit_path = token_stage_commit_path(&stage_path).unwrap();
        let stage_name = stage_path.file_name().unwrap();
        let commit_name = commit_path.file_name().unwrap();
        let pinned = open_private_parent(&active_path).unwrap();
        let mut prepared = prepare_token_stage(&pinned, stage_name, commit_name).unwrap();

        let displaced = directory.join(format!("guardian.token.displaced-{}", Uuid::new_v4()));
        std::fs::rename(&stage_path, &displaced).unwrap();
        let mut impostor = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&stage_path)
            .unwrap();
        impostor
            .write_all(&[0x44_u8; GUARDIAN_AUTH_TOKEN_BYTES])
            .unwrap();
        impostor.sync_all().unwrap();
        drop(impostor);

        assert!(
            validate_prepared_token_stage_binding(
                &pinned,
                stage_name,
                commit_name,
                &mut prepared,
            )
            .is_err()
        );
        assert!(!active_path.exists());
        assert!(displaced.exists());
    }

    #[test]
    fn provision_token_resumes_complete_and_partial_crash_stages() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-crash-stage-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();

        let complete_path = directory.join("complete.token");
        let complete_stage = token_stage_path(&complete_path).unwrap();
        let complete_bytes = [0x47_u8; GUARDIAN_AUTH_TOKEN_BYTES];
        let mut complete = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&complete_stage)
            .unwrap();
        complete.write_all(&complete_bytes).unwrap();
        complete.sync_all().unwrap();
        drop(complete);
        let complete_commit = token_stage_commit_path(&complete_stage).unwrap();
        let mut commit = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&complete_commit)
            .unwrap();
        commit
            .write_all(&token_stage_commit_record(&complete_bytes))
            .unwrap();
        commit.sync_all().unwrap();
        drop(commit);

        assert_eq!(
            provision_guardian_token(&complete_path).unwrap(),
            ProvisionTokenOutcome::Created
        );
        assert_eq!(std::fs::read(&complete_path).unwrap(), complete_bytes);
        assert!(matches!(
            std::fs::symlink_metadata(&complete_stage),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));

        let partial_path = directory.join("partial.token");
        let partial_stage = token_stage_path(&partial_path).unwrap();
        let mut partial = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&partial_stage)
            .unwrap();
        partial.write_all(b"partial-crash-cut").unwrap();
        partial.sync_all().unwrap();
        drop(partial);
        let partial_commit = token_stage_commit_path(&partial_stage).unwrap();
        let mut commit = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&partial_commit)
            .unwrap();
        commit.write_all(b"torn-commit").unwrap();
        commit.sync_all().unwrap();
        drop(commit);

        assert_eq!(
            provision_guardian_token(&partial_path).unwrap(),
            ProvisionTokenOutcome::Created
        );
        let resumed = std::fs::read(&partial_path).unwrap();
        assert_eq!(resumed.len(), GUARDIAN_AUTH_TOKEN_BYTES);
        assert!(resumed.iter().any(|byte| *byte != 0));
        assert!(matches!(
            std::fs::symlink_metadata(&partial_stage),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));
        assert_eq!(
            std::fs::read(&partial_commit).unwrap(),
            token_stage_commit_record(&resumed)
        );
    }

    #[test]
    fn provision_token_rejects_malicious_stage_without_activating_it() {
        let canonical_temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("frankenterm-guardian-token-malicious-stage-")
            .tempdir_in(canonical_temp)
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();

        let symlink_active = directory.join("symlink.token");
        let symlink_stage = token_stage_path(&symlink_active).unwrap();
        let symlink_target = directory.join("symlink-target");
        let mut target = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&symlink_target)
            .unwrap();
        target.write_all(&[0x31; GUARDIAN_AUTH_TOKEN_BYTES]).unwrap();
        target.sync_all().unwrap();
        drop(target);
        symlink(&symlink_target, &symlink_stage).unwrap();
        assert!(matches!(
            provision_guardian_token(&symlink_active),
            Err(GuardianServiceError::FilesystemSecurity(_))
        ));
        assert!(matches!(
            std::fs::symlink_metadata(&symlink_active),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));
        assert!(std::fs::symlink_metadata(&symlink_stage)
            .unwrap()
            .file_type()
            .is_symlink());

        let oversized_active = directory.join("oversized.token");
        let oversized_stage = token_stage_path(&oversized_active).unwrap();
        let oversized_bytes = [0x52_u8; GUARDIAN_AUTH_TOKEN_BYTES + 1];
        let mut oversized = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&oversized_stage)
            .unwrap();
        oversized.write_all(&oversized_bytes).unwrap();
        oversized.sync_all().unwrap();
        drop(oversized);
        assert!(matches!(
            provision_guardian_token(&oversized_active),
            Err(GuardianServiceError::FilesystemSecurity(_))
        ));
        assert_eq!(std::fs::read(&oversized_stage).unwrap(), oversized_bytes);
        assert!(matches!(
            std::fs::symlink_metadata(&oversized_active),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));

        let hardlink_active = directory.join("hardlink.token");
        let hardlink_stage = token_stage_path(&hardlink_active).unwrap();
        let hardlink_source = directory.join("hardlink-source");
        let hardlink_bytes = [0x63_u8; GUARDIAN_AUTH_TOKEN_BYTES];
        let mut source = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&hardlink_source)
            .unwrap();
        source.write_all(&hardlink_bytes).unwrap();
        source.sync_all().unwrap();
        drop(source);
        std::fs::hard_link(&hardlink_source, &hardlink_stage).unwrap();
        assert!(matches!(
            provision_guardian_token(&hardlink_active),
            Err(GuardianServiceError::FilesystemSecurity(_))
        ));
        assert_eq!(std::fs::read(&hardlink_source).unwrap(), hardlink_bytes);
        assert!(matches!(
            std::fs::symlink_metadata(&hardlink_active),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));

        let commit_link_active = directory.join("commit-link.token");
        let commit_link_stage = token_stage_path(&commit_link_active).unwrap();
        let commit_link_marker = token_stage_commit_path(&commit_link_stage).unwrap();
        let commit_link_bytes = [0x74_u8; GUARDIAN_AUTH_TOKEN_BYTES];
        let mut stage = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&commit_link_stage)
            .unwrap();
        stage.write_all(&commit_link_bytes).unwrap();
        stage.sync_all().unwrap();
        drop(stage);
        symlink(&symlink_target, &commit_link_marker).unwrap();
        assert!(matches!(
            provision_guardian_token(&commit_link_active),
            Err(GuardianServiceError::FilesystemSecurity(_))
        ));
        assert!(matches!(
            std::fs::symlink_metadata(&commit_link_active),
            Err(error) if error.kind() == ErrorKind::NotFound
        ));
        assert!(std::fs::symlink_metadata(&commit_link_marker)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
