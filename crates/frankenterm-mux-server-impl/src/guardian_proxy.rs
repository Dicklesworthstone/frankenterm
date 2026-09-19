//! Concrete mux-side proxy objects for one already-claimed guardian pane.
//!
//! This module owns the mutation-sequence actor, consuming checkpoint/output
//! replay, the resumable replay-tail reader, and the portable-pty proxy facets.
//! [`GuardianDomain`] explicitly selects a configured guardian for same-session
//! Genesis births, claims and restores each pane before publication, and fences
//! further births after an unadopted outcome. It does not reconstruct successor
//! ownership or window/tab topology after restart. Replay first returns an
//! off-topology [`ActivatedGuardianProxy`]; registration remains a separate
//! cancellation-safe mux commit boundary.

use anyhow::Context as _;
use frankenterm_pty_guardian::{
    GuardianClaimedPaneLease, GuardianClient, GuardianClientError, GuardianDurableSpawnCustodyV1,
};
use mux::domain::{
    Domain, DomainId, DomainState, GuardianPanePublicationReceipt, LocalDomain, UnpublishedPane,
};
use mux::guardian_checkpoint::{
    GuardianRestoredParserPrefix, GuardianRestoredTerminal, GuardianSpawnCustodyScopeV1,
    LiveParserCheckpointAck, PublishedGuardianCheckpoint,
};
use mux::guardian_protocol::{
    GUARDIAN_MAX_INPUT_BYTES, GUARDIAN_MAX_PANES, GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES,
    GUARDIAN_MAX_REPLAY_RECORDS, GUARDIAN_MAX_REPLAY_WAIT_MILLIS, GuardianCensusEntry,
    GuardianCensusPaneStatus, GuardianCheckpointDescriptorV1, GuardianCheckpointDisposition,
    GuardianCheckpointIdentityDigest, GuardianCheckpointIntent, GuardianCheckpointOutputBoundaryV1,
    GuardianCheckpointReceipt, GuardianCheckpointScopeV1, GuardianCheckpointStageReplyV1,
    GuardianCheckpointStageRequestV1, GuardianInputEffectQuery, GuardianProtocolError,
    GuardianRejectionCode, GuardianReplayAckReceiptV1, GuardianReplayAckV1, GuardianReplayCursorV1,
    GuardianReplayDeliveryError, GuardianReplayGapReasonV1, GuardianReplayPageBodyDelivery,
    GuardianReplayPageDelivery, GuardianReplayRecordDelivery, GuardianReplayRequestV1,
    GuardianReplaySelectorV1, GuardianReply, InputEffectState,
};
use mux::localpane::{GuardianPaneLeaseControl, GuardianPaneLeaseIdentity, LocalPane};
use mux::pane::{
    GuardianLiveCheckpointPublisher, GuardianLiveOutputDelivery, GuardianLiveOutputReader, PaneId,
};
use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;
#[cfg(test)]
use wezterm_term::InertTerminal;
use wezterm_term::terminalstate::checkpoint::{TerminalCheckpointError, TerminalCheckpointLimits};
use wezterm_term::{InertTerminalError, Terminal, TerminalConfiguration};
use zeroize::{Zeroize as _, Zeroizing};

const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_CENSUS_REFRESH_ATTEMPTS: usize = 2;
const GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS: usize = 2;
const GUARDIAN_RESTORE_REOPEN_ATTEMPTS: usize = 2;
const GUARDIAN_RESTORE_MAX_PAGES: usize = 65_536;
const GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_REPLAY_IDLE_POLL_MAX_INTERVAL: Duration = Duration::from_millis(1_000);
const GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES: u32 = 64 * 1_024;
const GUARDIAN_CHECKPOINT_QUERY_ATTEMPTS: usize = 2;
const GUARDIAN_RETIREMENT_RETRY_MIN_INTERVAL: Duration = Duration::from_millis(50);
const GUARDIAN_RETIREMENT_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(5);

/// Explicit, same-incarnation guardian spawning. This is not a successor or
/// restart attachment domain: an unadopted birth fences further spawning.
pub struct GuardianDomain {
    commands: LocalDomain,
    owner: std::sync::Weak<mux::Mux>,
    mux_incarnation: Uuid,
    socket_path: PathBuf,
    token_path: PathBuf,
    admission: Arc<AtomicBool>,
    state: Arc<Mutex<GuardianDomainState>>,
}

#[derive(Default)]
struct GuardianDomainState {
    census: Option<Arc<GuardianCensusCoordinator>>,
    unadopted_birth: Option<(Uuid, Uuid, Uuid)>,
    publication: Option<GuardianPanePublicationReceipt>,
}

struct GuardianDomainSpawnAdmission(Arc<AtomicBool>);

struct GuardianDomainSpawnCancellation(Arc<AtomicBool>);

impl Drop for GuardianDomainSpawnCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Drop for GuardianDomainSpawnAdmission {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl GuardianDomain {
    pub fn new(
        mux: &Arc<mux::Mux>,
        socket_path: PathBuf,
        token_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let (session, _) = mux.topology_snapshot_authority()?;
        Ok(Self {
            commands: LocalDomain::new("guardian")?,
            owner: Arc::downgrade(mux),
            mux_incarnation: Uuid::from_bytes(session.as_bytes()),
            socket_path,
            token_path,
            admission: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(GuardianDomainState::default())),
        })
    }

    fn validate_owner(&self, mux: &Arc<mux::Mux>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, mux))
                && Uuid::from_bytes(mux.topology_snapshot_authority()?.0.as_bytes())
                    == self.mux_incarnation,
            "guardian domain belongs to another mux session"
        );
        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl Domain for GuardianDomain {
    async fn spawn_pane(
        &self,
        mux: &Arc<mux::Mux>,
        size: wezterm_term::TerminalSize,
        command: Option<portable_pty::CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn mux::pane::Pane>> {
        self.spawn_unpublished_pane(mux, size, command, command_dir)
            .await?
            .publish(mux)
    }

    async fn spawn_unpublished_pane(
        &self,
        mux: &Arc<mux::Mux>,
        size: wezterm_term::TerminalSize,
        command: Option<portable_pty::CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<UnpublishedPane> {
        self.validate_owner(mux)?;
        anyhow::ensure!(
            self.admission
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "guardian domain already has a spawn in flight"
        );
        let admission = GuardianDomainSpawnAdmission(Arc::clone(&self.admission));
        {
            let mut state = self.state.lock();
            if let Some((pane, request, effect)) = state.unadopted_birth {
                anyhow::ensure!(
                    state
                        .publication
                        .as_ref()
                        .is_some_and(|receipt| receipt.was_published()),
                    "guardian domain retains an unadopted birth: pane={pane} request={request} effect={effect}"
                );
                state.unadopted_birth = None;
                state.publication = None;
            }
        }
        let pane_id = mux::pane::alloc_pane_id()?;
        let command = self
            .commands
            .build_command(mux, command, command_dir, pane_id)
            .await?;
        self.validate_owner(mux)?;
        let domain_id = self.domain_id();
        let pane = Uuid::new_v4();
        let request = Uuid::new_v4();
        let effect = Uuid::new_v4();
        let description = "guardian-owned command".to_string();
        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(config::TermConfig::new_for_pane(
                pane_id,
                domain_id,
                *pane.as_bytes(),
                description.clone(),
            ));
        let pty_size = PtySize {
            rows: size.rows.try_into()?,
            cols: size.cols.try_into()?,
            pixel_width: size.pixel_width.try_into()?,
            pixel_height: size.pixel_height.try_into()?,
        };
        validate_pty_size(pty_size)?;
        let socket = self.socket_path.clone();
        let token = self.token_path.clone();
        let mux_incarnation = self.mux_incarnation;
        let state = Arc::clone(&self.state);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = GuardianDomainSpawnCancellation(Arc::clone(&cancelled));
        let result = promise::spawn::spawn_into_new_thread(move || {
            let _admission = admission;
            let started = Instant::now();
            let mut attempts = 0;
            let connect = |attempts: &mut usize, expected: Option<Uuid>, before_birth: bool| {
                loop {
                    anyhow::ensure!(
                        !before_birth || !cancelled.load(Ordering::Acquire),
                        "guardian spawn cancelled before connection"
                    );
                    anyhow::ensure!(
                        *attempts < 32 && started.elapsed() < Duration::from_secs(5),
                        "guardian birth connection retry admission exhausted"
                    );
                    *attempts += 1;
                    match GuardianClient::connect_for_genesis(&socket, &token, mux_incarnation) {
                        Ok(client) => {
                            anyhow::ensure!(
                                expected.is_none_or(
                                    |identity| identity == client.guardian_incarnation()
                                ),
                                "guardian incarnation changed during birth connection"
                            );
                            return Ok(client);
                        }
                        Err(GuardianClientError::Io(_))
                            if *attempts < 32 && started.elapsed() < Duration::from_secs(5) =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => return Err(anyhow::Error::new(error)),
                    }
                }
            };
            let expected_guardian = state
                .lock()
                .census
                .as_ref()
                .map(|census| census.guardian_incarnation());
            let mut client = connect(&mut attempts, expected_guardian, true)
                .context("connect guardian for Genesis birth")?;
            // Establish all avoidable connection state before creating a child.
            let census = {
                let mut state = state.lock();
                if let Some(census) = &state.census {
                    anyhow::ensure!(
                        census.guardian_incarnation() == client.guardian_incarnation(),
                        "guardian incarnation changed within this domain"
                    );
                    Arc::clone(census)
                } else {
                    let census = Arc::new(
                        GuardianCensusCoordinator::connect(
                            &socket,
                            &token,
                            client.guardian_incarnation(),
                            mux_incarnation,
                        )
                        .context("initialize guardian birth census")?,
                    );
                    state.census = Some(Arc::clone(&census));
                    census
                }
            };
            let checkpoint = Terminal::new(
                size,
                Arc::clone(&config),
                "FrankenTerm",
                config::wezterm_version(),
                Box::new(io::sink()),
            )
            .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
            .context("capture guardian initial terminal model")?;
            let descriptor =
                GuardianCheckpointDescriptorV1::for_genesis_artifact(effect, &checkpoint)?;
            loop {
                anyhow::ensure!(
                    !cancelled.load(Ordering::Acquire),
                    "guardian spawn cancelled before checkpoint staging"
                );
                anyhow::ensure!(
                    attempts < 32 && started.elapsed() < Duration::from_secs(5),
                    "guardian checkpoint staging retry admission exhausted"
                );
                match client.stage_genesis_checkpoint(
                    effect,
                    descriptor,
                    checkpoint.canonical_payload(),
                    GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES,
                ) {
                    Ok(_) => break,
                    Err(GuardianClientError::Io(_))
                        if attempts < 32 && started.elapsed() < Duration::from_secs(5) =>
                    {
                        // Begin authenticates an existing candidate and returns
                        // its durable prefix. The helper retains the exact upload
                        // identity, skips that prefix, and ends with Query.
                        thread::sleep(Duration::from_millis(10));
                        client = connect(&mut attempts, Some(census.guardian_incarnation()), true)
                            .context("reconnect guardian for checkpoint staging")?;
                    }
                    Err(error) => {
                        return Err(
                            anyhow::Error::new(error).context("stage guardian Genesis checkpoint")
                        );
                    }
                }
            }
            anyhow::ensure!(
                !cancelled.load(Ordering::Acquire),
                "guardian spawn cancelled before birth"
            );
            anyhow::ensure!(
                attempts < 32 && started.elapsed() < Duration::from_secs(5),
                "guardian birth retry admission exhausted before Spawn"
            );
            // From this point a lost response may hide a real child. Keep exact
            // identities until ownership has reached the cancellation-safe guard.
            state.lock().unadopted_birth = Some((pane, request, effect));
            let reply = loop {
                anyhow::ensure!(
                    attempts < 32 && started.elapsed() < Duration::from_secs(5),
                    "guardian exact birth reconciliation retry admission exhausted"
                );
                attempts += 1;
                match client.spawn(pane, request, effect, command.clone(), pty_size) {
                    Ok(reply) => break reply,
                    Err(GuardianClientError::Io(_))
                        if attempts < 32 && started.elapsed() < Duration::from_secs(5) =>
                    {
                        // Only replay the identical idempotent request after transport
                        // loss. Each exchange keeps its own transport timeout; this
                        // elapsed limit admits retries, not a hard total deadline.
                        thread::sleep(Duration::from_millis(10));
                        client = connect(&mut attempts, Some(census.guardian_incarnation()), false)
                            .context("reconnect guardian for exact birth reconciliation")?;
                        anyhow::ensure!(
                            client.guardian_incarnation() == census.guardian_incarnation(),
                            "guardian incarnation changed during birth reconciliation"
                        );
                    }
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context("submit guardian birth"));
                    }
                }
            };
            anyhow::ensure!(
                reply
                    == (GuardianReply::Spawned {
                        pane_id: pane,
                        generation: 0
                    }),
                "guardian returned an unexpected birth receipt"
            );
            // Hello authenticates the guardian UUID, not its or the broker's
            // build. Recover those nonsecret fields only from private AEAD
            // custody, binding all identities known by this successful birth.
            let custody = GuardianDurableSpawnCustodyV1::open_existing_for_birth(
                &token,
                client.guardian_incarnation(),
                mux_incarnation,
                pane,
                effect,
                frankenterm_pty_guardian::guardian_runtime_build_identity()
                    .context("guardian birth requires the compiled mux build")?
                    .into_bytes(),
            )
            .context("authenticate existing guardian birth custody")?;
            let activated = GuardianProxyLeasePlan::prepare(&socket, &token, pty_size, census)
                .context("prepare guardian birth lease")?
                .with_spawn_custody(custody)?
                .claim(pane, 0, Uuid::new_v4(), Uuid::new_v4())
                .context("claim guardian birth lease")?
                .restore_and_activate(config, TerminalCheckpointLimits::default())
                .context("restore and activate guardian birth")?;
            let unpublished = UnpublishedPane::from_guardian_proxy(activated.into_local_pane(
                pane_id,
                domain_id,
                description,
            ))?;
            // The read-only receipt can become true only after actual mux
            // publication. Cancellation keeps it false; successful publication
            // remains remembered even after a short-lived pane is pruned.
            state.lock().publication = unpublished.guardian_publication_receipt();
            Ok(unpublished)
        })
        .await;
        drop(cancellation);
        result
    }

    fn domain_id(&self) -> DomainId {
        self.commands.domain_id()
    }
    fn domain_name(&self) -> &str {
        self.commands.domain_name()
    }
    fn detachable(&self) -> bool {
        false
    }
    fn detach(&self) -> anyhow::Result<()> {
        anyhow::bail!(
            "guardian domain detach is not implemented; pane lease retirement remains explicit"
        )
    }
    fn state(&self) -> DomainState {
        DomainState::Attached
    }
    async fn attach(
        &self,
        mux: &Arc<mux::Mux>,
        _owner: Option<Arc<mux::client::ClientId>>,
        _window: Option<mux::window::WindowId>,
    ) -> anyhow::Result<()> {
        self.validate_owner(mux)
    }
}

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
    #[error("guardian terminal activation failed before topology publication: {0}")]
    TerminalActivation(#[source] InertTerminalError),
    #[error("guardian restored model and output prefix validation failed")]
    RestoredModel(#[source] anyhow::Error),
    #[error("guardian mutation outcome is indeterminate; the lease is quarantined")]
    MutationOutcomeIndeterminate,
    #[error("guardian returned a reply inconsistent with the pending mutation")]
    UnexpectedMutationReply,
    #[error("guardian checkpoint staging returned an inconsistent durable state")]
    CheckpointStageInvariant,
    #[error("guardian checkpoint staging was quarantined")]
    CheckpointStageQuarantined,
    #[error("guardian checkpoint staging expired before durable acknowledgement")]
    CheckpointStageExpired,
    #[error("guardian checkpoint staging exceeded a bounded resource")]
    CheckpointStageCapacity,
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
    #[error("guardian retained retirement queue reached its pane bound")]
    RetirementCapacity,
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
            | GuardianProxyError::TerminalActivation(_) => {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingCheckpointMutation {
    sequence: u64,
    request_id: Uuid,
    effect_id: Uuid,
    intent: GuardianCheckpointIntent,
}

#[derive(Clone, Debug)]
enum PendingMutation {
    Input(PendingInput),
    Generic(PendingGenericMutation),
    Checkpoint(PendingCheckpointMutation),
}

impl PendingMutation {
    fn matches_generic(&self, kind: GenericMutation) -> bool {
        matches!(self, Self::Generic(pending) if pending.kind == kind)
    }

    fn matches_input(&self, payload: &[u8]) -> bool {
        matches!(self, Self::Input(pending) if pending.matches_payload(payload))
    }

    fn matches_checkpoint(&self, intent: GuardianCheckpointIntent) -> bool {
        matches!(self, Self::Checkpoint(pending) if pending.intent == intent)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveredPendingMutation {
    Generic,
    Checkpoint(GuardianCheckpointReceipt),
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

    #[allow(clippy::too_many_arguments)]
    fn checkpoint(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        intent: GuardianCheckpointIntent,
    ) -> Result<GuardianCheckpointReceipt, GuardianMutationTransportError>;

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

/// Independent checkpoint-stage channel for one exact guardian lease.
///
/// Staging may perform bounded disk I/O and lost-reply queries, so it must not
/// borrow the mutation actor's framed client while ordinary input is active.
trait GuardianCheckpointStageTransport: Send {
    fn checkpoint_stage(
        &mut self,
        request_id: Uuid,
        request: GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianProxyError>;
}

#[cfg(test)]
struct DormantCheckpointStageTransport;

#[cfg(test)]
impl GuardianCheckpointStageTransport for DormantCheckpointStageTransport {
    fn checkpoint_stage(
        &mut self,
        _request_id: Uuid,
        _request: GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianProxyError> {
        Err(GuardianProxyError::CheckpointStageInvariant)
    }
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
    retirement_slots: HashMap<Uuid, GuardianRetirementSlot>,
    next_retirement_order: u64,
}

struct GuardianRetirementSlot {
    identity: GuardianPaneLeaseIdentity,
    authority: Arc<Mutex<Option<GuardianCleanupAuthority>>>,
    retry_attempts: u32,
    retry_not_before: Instant,
    retry_in_flight: bool,
    retry_blocked: bool,
    order: u64,
}

enum GuardianCleanupAuthority {
    Claimed(SharedGuardianPaneLeaseActor),
    PendingClaim(Box<GuardianPendingClaim>),
}

/// An unresolved request, not proof that the guardian granted a lease.
struct GuardianPendingClaim {
    socket_path: PathBuf,
    token_path: PathBuf,
    identity: GuardianPaneLeaseIdentity,
    observed_generation: u64,
    request_id: Uuid,
    effect_id: Uuid,
    size: PtySize,
    spawn_custody: Option<GuardianSpawnCustodyScopeV1>,
    successor_custody: Option<mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1>,
}

impl GuardianPendingClaim {
    fn connect(&self) -> Result<GuardianClient, GuardianProxyError> {
        let client = if self.spawn_custody.is_some() && self.observed_generation == 1 {
            GuardianClient::connect_for_genesis(
                &self.socket_path,
                &self.token_path,
                self.identity.mux_incarnation(),
            )
        } else {
            GuardianClient::connect(
                &self.socket_path,
                &self.token_path,
                self.identity.mux_incarnation(),
            )
        }
        .map_err(map_replay_client_error)?;
        if client.guardian_incarnation() != self.identity.guardian_incarnation() {
            return Err(GuardianProxyError::GuardianIncarnationChanged);
        }
        Ok(client)
    }

    fn claim(
        &self,
        client: GuardianClient,
    ) -> Result<GuardianClaimedPaneLease, GuardianClientError> {
        if let Some(context) = self.successor_custody {
            drop(client);
            let custody =
                frankenterm_pty_guardian::GuardianDurableSuccessorCustodyV1::open_existing(
                    &self.token_path,
                    context.scope(),
                )
                .map_err(|_| {
                    GuardianClientError::Setup(
                        frankenterm_pty_guardian::GuardianServiceError::OutputInitialization,
                    )
                })?;
            if custody.context() != context {
                return Err(GuardianClientError::Setup(
                    frankenterm_pty_guardian::GuardianServiceError::OutputInitialization,
                ));
            }
            return GuardianClient::connect_for_successor_rotation(
                &self.socket_path,
                &self.token_path,
                self.identity.mux_incarnation(),
            )?
            .claim_mux_successor(custody, self.request_id, self.effect_id);
        }
        if let Some(scope) = self.spawn_custody.filter(|_| self.observed_generation == 1) {
            let custody = GuardianDurableSpawnCustodyV1::open_existing(&self.token_path, scope)
                .map_err(|_| {
                    GuardianClientError::Setup(
                        frankenterm_pty_guardian::GuardianServiceError::OutputInitialization,
                    )
                })?;
            return client.claim_mux_successor(custody, self.request_id, self.effect_id);
        }
        client.claim(
            self.identity.pane_id(),
            self.observed_generation,
            self.request_id,
            self.effect_id,
        )
    }

    fn recover(&self) -> Result<SharedGuardianPaneLeaseActor, GuardianProxyError> {
        let lease = self
            .claim(self.connect()?)
            .map_err(map_replay_client_error)?;
        let next_sequence = lease.next_sequence();
        let transport = GuardianClientTransport::from_claimed_lease(
            &self.socket_path,
            &self.token_path,
            self.identity,
            lease,
        );
        Ok(Arc::new(Mutex::new(
            GuardianPaneLeaseActor::with_validated_transport(
                self.identity,
                next_sequence,
                self.size,
                Box::new(transport),
            ),
        )))
    }
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
            .field("retirement_slots", &state.retirement_slots.len())
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
                retirement_slots: HashMap::new(),
                next_retirement_order: 0,
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

    fn reserve_retirement(
        self: &Arc<Self>,
        identity: GuardianPaneLeaseIdentity,
    ) -> Result<GuardianRetirementReservation, GuardianProxyError> {
        self.ensure_binding(identity)?;
        let mut state = self.state.lock();
        if state.retirement_slots.contains_key(&identity.pane_id()) {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian pane already has reserved retirement authority",
            ));
        }
        if state.retirement_slots.len() >= GUARDIAN_MAX_PANES {
            return Err(GuardianProxyError::RetirementCapacity);
        }
        state
            .retirement_slots
            .try_reserve(1)
            .map_err(|_| GuardianProxyError::CensusAllocation)?;
        let order = state.next_retirement_order;
        state.next_retirement_order = state
            .next_retirement_order
            .checked_add(1)
            .ok_or(GuardianProxyError::SequenceExhausted)?;
        let authority = Arc::new(Mutex::new(None));
        state.retirement_slots.insert(
            identity.pane_id(),
            GuardianRetirementSlot {
                identity,
                authority: Arc::clone(&authority),
                retry_attempts: 0,
                retry_not_before: Instant::now(),
                retry_in_flight: false,
                retry_blocked: false,
                order,
            },
        );
        Ok(GuardianRetirementReservation {
            coordinator: Arc::clone(self),
            identity,
            authority,
            active: true,
        })
    }

    fn cancel_retirement_reservation(&self, identity: GuardianPaneLeaseIdentity) {
        let mut state = self.state.lock();
        let removable = state
            .retirement_slots
            .get(&identity.pane_id())
            .is_some_and(|slot| {
                slot.identity == identity
                    && slot.authority.lock().is_none()
                    && !slot.retry_in_flight
            });
        if removable {
            state.retirement_slots.remove(&identity.pane_id());
        }
    }

    /// Retry at most one retained unpublished-lease retirement.
    ///
    /// The actor preserves the exact pending Retire request, effect, and
    /// mutation sequence. A failed maintenance call leaves the same authority
    /// in the bounded coordinator slot for a later attempt. An unresolved
    /// Claim first retries its original identity; only the authenticated
    /// successful reply can supply an actor to retire.
    pub fn retry_retained_lease_cleanup(&self) -> Result<bool, GuardianProxyError> {
        let now = Instant::now();
        let candidate = {
            let mut state = self.state.lock();
            let pane_id = state
                .retirement_slots
                .iter()
                .filter(|(_, slot)| {
                    slot.authority.lock().is_some()
                        && !slot.retry_in_flight
                        && !slot.retry_blocked
                        && slot.retry_not_before <= now
                })
                .min_by_key(|(_, slot)| slot.order)
                .map(|(pane_id, _)| *pane_id);
            pane_id.and_then(|pane_id| {
                let slot = state.retirement_slots.get_mut(&pane_id)?;
                slot.retry_in_flight = true;
                Some((pane_id, slot.identity, Arc::clone(&slot.authority)))
            })
        };
        let Some((pane_id, identity, authority)) = candidate else {
            return Ok(false);
        };

        let mut unresolved_claim = false;
        let result = {
            let mut retained = authority.lock();
            match retained.as_mut() {
                Some(GuardianCleanupAuthority::PendingClaim(pending)) => match pending.recover() {
                    Ok(actor) => {
                        // Store the genuine lease actor before Retire: a lost
                        // retirement reply must retain its exact mutation ledger.
                        *retained = Some(GuardianCleanupAuthority::Claimed(Arc::clone(&actor)));
                        actor.lock().retire(identity)
                    }
                    Err(error) => {
                        unresolved_claim = true;
                        Err(error)
                    }
                },
                Some(GuardianCleanupAuthority::Claimed(actor)) => actor.lock().retire(identity),
                None => Err(GuardianProxyError::InvalidConfiguration(
                    "retained guardian cleanup authority disappeared",
                )),
            }
        };
        let mut state = self.state.lock();
        let still_exact = state.retirement_slots.get(&pane_id).is_some_and(|slot| {
            slot.identity == identity
                && slot.retry_in_flight
                && Arc::ptr_eq(&slot.authority, &authority)
        });
        if !still_exact {
            return Err(GuardianProxyError::InvalidConfiguration(
                "retained guardian retirement authority changed during retry",
            ));
        }
        match result {
            Ok(()) => {
                state.retirement_slots.remove(&pane_id);
                state.cache = None;
                metrics::counter!(
                    "mux.guardian_proxy.retained_retirement_retry_total",
                    "outcome" => "retired",
                )
                .increment(1);
                Ok(true)
            }
            Err(error) if !unresolved_claim && guardian_retirement_is_resolved(&error) => {
                state.retirement_slots.remove(&pane_id);
                state.cache = None;
                log::warn!(
                    "retained guardian lease retirement no longer has live authority and is resolved without another mutation: {error}"
                );
                metrics::counter!(
                    "mux.guardian_proxy.retained_retirement_retry_total",
                    "outcome" => "already_absent",
                )
                .increment(1);
                Ok(true)
            }
            Err(error) => {
                let order = state.next_retirement_order;
                let next_order = state.next_retirement_order.checked_add(1);
                let retry_safe = guardian_cleanup_retry_is_safe(&error);
                if retry_safe {
                    let Some(next_order) = next_order else {
                        let Some(slot) = state.retirement_slots.get_mut(&pane_id) else {
                            return Err(GuardianProxyError::InvalidConfiguration(
                                "retained guardian retirement slot disappeared after retry",
                            ));
                        };
                        slot.retry_in_flight = false;
                        slot.retry_blocked = true;
                        return Err(GuardianProxyError::SequenceExhausted);
                    };
                    state.next_retirement_order = next_order;
                }
                let Some(slot) = state.retirement_slots.get_mut(&pane_id) else {
                    return Err(GuardianProxyError::InvalidConfiguration(
                        "retained guardian retirement slot disappeared after retry",
                    ));
                };
                slot.retry_in_flight = false;
                if !retry_safe {
                    slot.retry_blocked = true;
                    metrics::counter!(
                        "mux.guardian_proxy.retained_retirement_retry_total",
                        "outcome" => "blocked",
                    )
                    .increment(1);
                    return Err(error);
                }
                slot.retry_attempts = slot.retry_attempts.saturating_add(1);
                slot.retry_not_before =
                    Instant::now() + guardian_retirement_retry_delay(slot.retry_attempts);
                slot.order = order;
                metrics::counter!(
                    "mux.guardian_proxy.retained_retirement_retry_total",
                    "outcome" => "retained",
                )
                .increment(1);
                Err(error)
            }
        }
    }

    /// Number of pre-reserved or retained unpublished-lease cleanup slots.
    #[must_use]
    pub fn retained_lease_cleanup_count(&self) -> usize {
        self.state.lock().retirement_slots.len()
    }

    /// Number of retained cleanup authorities requiring operator intervention.
    #[must_use]
    pub fn blocked_retained_lease_cleanup_count(&self) -> usize {
        self.state
            .lock()
            .retirement_slots
            .values()
            .filter(|slot| slot.retry_blocked)
            .count()
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
        if let Err(error) = self.retry_retained_lease_cleanup() {
            log::warn!(
                "retained guardian lease retirement remains pending after bounded census maintenance: {error}"
            );
        }
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

struct GuardianRetirementReservation {
    coordinator: Arc<GuardianCensusCoordinator>,
    identity: GuardianPaneLeaseIdentity,
    authority: Arc<Mutex<Option<GuardianCleanupAuthority>>>,
    active: bool,
}

impl GuardianRetirementReservation {
    fn release(&mut self) {
        if self.active {
            self.coordinator
                .cancel_retirement_reservation(self.identity);
            self.active = false;
        }
    }

    fn retain(self, actor: SharedGuardianPaneLeaseActor, retry_blocked: bool) {
        self.retain_authority(GuardianCleanupAuthority::Claimed(actor), retry_blocked);
    }

    fn retain_authority(mut self, authority: GuardianCleanupAuthority, retry_blocked: bool) {
        let replaced = self.authority.lock().replace(authority);
        debug_assert!(replaced.is_none());
        let slot_updated = {
            let mut state = self.coordinator.state.lock();
            state
                .retirement_slots
                .get_mut(&self.identity.pane_id())
                .filter(|slot| {
                    slot.identity == self.identity && Arc::ptr_eq(&slot.authority, &self.authority)
                })
                .map(|slot| slot.retry_blocked = retry_blocked)
                .is_some()
        };
        if !slot_updated {
            log::error!(
                "guardian retirement reservation disappeared while retaining exact cleanup authority"
            );
            self.active = false;
            std::mem::forget(self);
            return;
        }
        metrics::counter!("mux.guardian_proxy.retained_retirement_total").increment(1);
        self.active = false;
    }
}

impl Drop for GuardianRetirementReservation {
    fn drop(&mut self) {
        self.release();
    }
}

fn guardian_retirement_retry_delay(attempts: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(attempts.saturating_sub(1).min(7))
        .unwrap_or(u32::MAX);
    GUARDIAN_RETIREMENT_RETRY_MIN_INTERVAL
        .saturating_mul(multiplier)
        .min(GUARDIAN_RETIREMENT_RETRY_MAX_INTERVAL)
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

struct GuardianCheckpointStageClientTransport {
    socket_path: PathBuf,
    token_path: PathBuf,
    identity: GuardianPaneLeaseIdentity,
    client: Option<GuardianClient>,
}

impl GuardianCheckpointStageClientTransport {
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
            self.client = None;
        }
        result.map_err(map_replay_client_error)
    }
}

impl GuardianCheckpointStageTransport for GuardianCheckpointStageClientTransport {
    fn checkpoint_stage(
        &mut self,
        request_id: Uuid,
        request: GuardianCheckpointStageRequestV1,
    ) -> Result<GuardianCheckpointStageReplyV1, GuardianProxyError> {
        self.call(|client| client.checkpoint_stage(request_id, request))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardianCheckpointStageStatus {
    Absent,
    Progress(u32),
    Sealed(Uuid),
    Acked(Uuid),
    Expired,
    Quarantined,
}

struct PendingGuardianCheckpointPublication {
    scope: GuardianCheckpointScopeV1,
    upload_id: Uuid,
    descriptor: GuardianCheckpointDescriptorV1,
    chunk_bytes: u32,
    total_chunks: u32,
    capture: LiveParserCheckpointAck,
    begin_request_id: Uuid,
    chunk_request_ids: Vec<Uuid>,
    query_request_id: Uuid,
    seal_request_id: Uuid,
    ack_request_id: Uuid,
    completion_id: Option<Uuid>,
    adoption_receipt: Option<GuardianCheckpointReceipt>,
}

impl fmt::Debug for PendingGuardianCheckpointPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingGuardianCheckpointPublication")
            .field("scope", &self.scope)
            .field("upload_id", &self.upload_id)
            .field("descriptor", &self.descriptor)
            .field("chunk_bytes", &self.chunk_bytes)
            .field("total_chunks", &self.total_chunks)
            .field("terminal_payload", &"[REDACTED]")
            .field("completion_id", &self.completion_id)
            .field("adoption_receipt", &self.adoption_receipt)
            .finish_non_exhaustive()
    }
}

impl PendingGuardianCheckpointPublication {
    fn from_live_capture(
        identity: GuardianPaneLeaseIdentity,
        capture: LiveParserCheckpointAck,
    ) -> Result<Self, GuardianProxyError> {
        if capture.durable_pane_id() != identity.pane_id() {
            return Err(GuardianProxyError::LeaseIdentityMismatch);
        }
        let descriptor =
            GuardianCheckpointDescriptorV1::from_live_capture(&capture, identity.generation())
                .map_err(GuardianProxyError::ReplayProtocol)?;
        if u64::try_from(capture.terminal_checkpoint().canonical_payload().len())
            != Ok(descriptor.total_bytes())
        {
            return Err(GuardianProxyError::CheckpointStageInvariant);
        }
        let total_chunks_u64 = descriptor
            .total_bytes()
            .div_ceil(u64::from(GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES));
        let total_chunks = u32::try_from(total_chunks_u64)
            .map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        if total_chunks == 0 {
            return Err(GuardianProxyError::CheckpointStageInvariant);
        }
        let total_chunks_usize = usize::try_from(total_chunks)
            .map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        let mut chunk_request_ids = Vec::new();
        chunk_request_ids
            .try_reserve_exact(total_chunks_usize)
            .map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        chunk_request_ids.extend((0..total_chunks).map(|_| Uuid::new_v4()));
        Ok(Self {
            scope: GuardianCheckpointScopeV1::Pane {
                pane_id: identity.pane_id(),
                generation: identity.generation(),
            },
            upload_id: Uuid::new_v4(),
            descriptor,
            chunk_bytes: GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES,
            total_chunks,
            capture,
            begin_request_id: Uuid::new_v4(),
            chunk_request_ids,
            query_request_id: Uuid::new_v4(),
            seal_request_id: Uuid::new_v4(),
            ack_request_id: Uuid::new_v4(),
            completion_id: None,
            adoption_receipt: None,
        })
    }

    fn classify_reply(
        &self,
        reply: GuardianCheckpointStageReplyV1,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        if reply.upload_id() != self.upload_id {
            return Err(GuardianProxyError::CheckpointStageInvariant);
        }
        match reply {
            GuardianCheckpointStageReplyV1::Absent { .. } => {
                Ok(GuardianCheckpointStageStatus::Absent)
            }
            GuardianCheckpointStageReplyV1::Ready {
                next_index,
                committed_bytes,
                ..
            }
            | GuardianCheckpointStageReplyV1::Progress {
                next_index,
                committed_bytes,
                ..
            } => {
                if next_index > self.total_chunks {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
                let expected_bytes = u64::from(next_index)
                    .checked_mul(u64::from(self.chunk_bytes))
                    .ok_or(GuardianProxyError::CheckpointStageCapacity)?
                    .min(self.descriptor.total_bytes());
                if committed_bytes != expected_bytes {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
                Ok(GuardianCheckpointStageStatus::Progress(next_index))
            }
            GuardianCheckpointStageReplyV1::Sealed {
                completion_id,
                checkpoint_id,
                boundary_id,
                total_bytes,
                ..
            }
            | GuardianCheckpointStageReplyV1::Acked {
                completion_id,
                checkpoint_id,
                boundary_id,
                total_bytes,
                ..
            } => {
                if checkpoint_id != self.descriptor.checkpoint_id()
                    || boundary_id != self.descriptor.boundary_id()
                    || total_bytes != self.descriptor.total_bytes()
                {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
                if matches!(reply, GuardianCheckpointStageReplyV1::Sealed { .. }) {
                    Ok(GuardianCheckpointStageStatus::Sealed(completion_id))
                } else {
                    Ok(GuardianCheckpointStageStatus::Acked(completion_id))
                }
            }
            GuardianCheckpointStageReplyV1::Expired {
                checkpoint_id,
                boundary_id,
                total_bytes,
                ..
            } => {
                if checkpoint_id != self.descriptor.checkpoint_id()
                    || boundary_id != self.descriptor.boundary_id()
                    || total_bytes != self.descriptor.total_bytes()
                {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
                Ok(GuardianCheckpointStageStatus::Expired)
            }
            GuardianCheckpointStageReplyV1::Quarantined { .. } => {
                Ok(GuardianCheckpointStageStatus::Quarantined)
            }
        }
    }

    fn query(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        let mut last_error = None;
        for _ in 0..GUARDIAN_CHECKPOINT_QUERY_ATTEMPTS {
            let request = GuardianCheckpointStageRequestV1::query(
                self.scope,
                self.upload_id,
                self.descriptor,
                self.chunk_bytes,
            )
            .map_err(GuardianProxyError::ReplayProtocol)?;
            match transport.checkpoint_stage(self.query_request_id, request) {
                Ok(reply) => return self.classify_reply(reply),
                Err(error) if replay_error_is_retryable_io(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or(GuardianProxyError::CheckpointStageInvariant))
    }

    fn classify_exchange(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
        result: Result<GuardianCheckpointStageReplyV1, GuardianProxyError>,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        match result {
            Ok(reply) => self.classify_reply(reply),
            Err(error) if replay_error_is_retryable_io(&error) => self.query(transport),
            Err(error) => Err(error),
        }
    }

    fn send_begin(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        let request = GuardianCheckpointStageRequestV1::begin(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
        )
        .map_err(GuardianProxyError::ReplayProtocol)?;
        let result = transport.checkpoint_stage(self.begin_request_id, request);
        self.classify_exchange(transport, result)
    }

    fn send_chunk(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
        index: u32,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        let start_u64 = u64::from(index)
            .checked_mul(u64::from(self.chunk_bytes))
            .ok_or(GuardianProxyError::CheckpointStageCapacity)?;
        let end_u64 = start_u64
            .checked_add(u64::from(self.chunk_bytes))
            .ok_or(GuardianProxyError::CheckpointStageCapacity)?
            .min(self.descriptor.total_bytes());
        let start =
            usize::try_from(start_u64).map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        let end =
            usize::try_from(end_u64).map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        let request_id = *self
            .chunk_request_ids
            .get(usize::try_from(index).map_err(|_| GuardianProxyError::CheckpointStageCapacity)?)
            .ok_or(GuardianProxyError::CheckpointStageInvariant)?;
        let mut bytes = Zeroizing::new(Vec::new());
        bytes
            .try_reserve_exact(end.saturating_sub(start))
            .map_err(|_| GuardianProxyError::CheckpointStageCapacity)?;
        bytes.extend_from_slice(
            self.capture
                .terminal_checkpoint()
                .canonical_payload()
                .get(start..end)
                .ok_or(GuardianProxyError::CheckpointStageInvariant)?,
        );
        let request = GuardianCheckpointStageRequestV1::chunk(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
            index,
            bytes,
        )
        .map_err(GuardianProxyError::ReplayProtocol)?;
        let result = transport.checkpoint_stage(request_id, request);
        self.classify_exchange(transport, result)
    }

    fn send_seal(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        let request = GuardianCheckpointStageRequestV1::seal(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
        )
        .map_err(GuardianProxyError::ReplayProtocol)?;
        let result = transport.checkpoint_stage(self.seal_request_id, request);
        self.classify_exchange(transport, result)
    }

    fn send_ack(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
        completion_id: Uuid,
    ) -> Result<GuardianCheckpointStageStatus, GuardianProxyError> {
        let request = GuardianCheckpointStageRequestV1::ack(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
            completion_id,
        )
        .map_err(GuardianProxyError::ReplayProtocol)?;
        let result = transport.checkpoint_stage(self.ack_request_id, request);
        self.classify_exchange(transport, result)
    }

    fn drive_to_sealed(
        &mut self,
        transport: &mut dyn GuardianCheckpointStageTransport,
    ) -> Result<Uuid, GuardianProxyError> {
        let mut status = self.query(transport)?;
        let max_steps = usize::try_from(self.total_chunks)
            .map_err(|_| GuardianProxyError::CheckpointStageCapacity)?
            .checked_mul(2)
            .and_then(|steps| steps.checked_add(8))
            .ok_or(GuardianProxyError::CheckpointStageCapacity)?;
        for _ in 0..max_steps {
            status = match status {
                GuardianCheckpointStageStatus::Absent => self.send_begin(transport)?,
                GuardianCheckpointStageStatus::Progress(next_index)
                    if next_index < self.total_chunks =>
                {
                    self.send_chunk(transport, next_index)?
                }
                GuardianCheckpointStageStatus::Progress(next_index)
                    if next_index == self.total_chunks =>
                {
                    self.send_seal(transport)?
                }
                GuardianCheckpointStageStatus::Sealed(completion_id) => {
                    if self
                        .completion_id
                        .is_some_and(|prior| prior != completion_id)
                    {
                        return Err(GuardianProxyError::CheckpointStageInvariant);
                    }
                    self.completion_id = Some(completion_id);
                    return Ok(completion_id);
                }
                GuardianCheckpointStageStatus::Acked(completion_id) => {
                    if self.adoption_receipt.is_none()
                        || self
                            .completion_id
                            .is_some_and(|prior| prior != completion_id)
                    {
                        return Err(GuardianProxyError::CheckpointStageInvariant);
                    }
                    self.completion_id = Some(completion_id);
                    return Ok(completion_id);
                }
                GuardianCheckpointStageStatus::Expired => {
                    return Err(GuardianProxyError::CheckpointStageExpired);
                }
                GuardianCheckpointStageStatus::Quarantined => {
                    return Err(GuardianProxyError::CheckpointStageQuarantined);
                }
                GuardianCheckpointStageStatus::Progress(_) => {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
            };
        }
        Err(GuardianProxyError::CheckpointStageInvariant)
    }

    fn drive_ack(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
        completion_id: Uuid,
    ) -> Result<(), GuardianProxyError> {
        let mut status = self.send_ack(transport, completion_id)?;
        for _ in 0..GUARDIAN_CHECKPOINT_QUERY_ATTEMPTS {
            match status {
                GuardianCheckpointStageStatus::Acked(observed) if observed == completion_id => {
                    return Ok(());
                }
                GuardianCheckpointStageStatus::Sealed(observed) if observed == completion_id => {
                    status = self.send_ack(transport, completion_id)?;
                }
                GuardianCheckpointStageStatus::Expired => {
                    return Err(GuardianProxyError::CheckpointStageExpired);
                }
                GuardianCheckpointStageStatus::Quarantined => {
                    return Err(GuardianProxyError::CheckpointStageQuarantined);
                }
                GuardianCheckpointStageStatus::Absent
                | GuardianCheckpointStageStatus::Progress(_)
                | GuardianCheckpointStageStatus::Sealed(_)
                | GuardianCheckpointStageStatus::Acked(_) => {
                    return Err(GuardianProxyError::CheckpointStageInvariant);
                }
            }
        }
        Err(GuardianProxyError::CheckpointStageInvariant)
    }

    fn intent(&self) -> GuardianCheckpointIntent {
        GuardianCheckpointIntent::new(
            self.descriptor.checkpoint_id(),
            self.descriptor.boundary_id(),
        )
    }
}

#[derive(Clone, Copy)]
struct AcknowledgedGuardianCheckpointPublication {
    receipt: GuardianCheckpointReceipt,
    scope: GuardianCheckpointScopeV1,
    upload_id: Uuid,
    descriptor: GuardianCheckpointDescriptorV1,
    chunk_bytes: u32,
    query_request_id: Uuid,
    completion_id: Uuid,
}

impl AcknowledgedGuardianCheckpointPublication {
    fn from_pending(
        pending: &PendingGuardianCheckpointPublication,
        receipt: GuardianCheckpointReceipt,
    ) -> Result<Self, GuardianProxyError> {
        Ok(Self {
            receipt,
            scope: pending.scope,
            upload_id: pending.upload_id,
            descriptor: pending.descriptor,
            chunk_bytes: pending.chunk_bytes,
            query_request_id: pending.query_request_id,
            completion_id: pending
                .completion_id
                .ok_or(GuardianProxyError::CheckpointStageInvariant)?,
        })
    }

    fn confirm_ack(
        &self,
        transport: &mut dyn GuardianCheckpointStageTransport,
    ) -> Result<(), GuardianProxyError> {
        let request = GuardianCheckpointStageRequestV1::query(
            self.scope,
            self.upload_id,
            self.descriptor,
            self.chunk_bytes,
        )
        .map_err(GuardianProxyError::ReplayProtocol)?;
        match transport.checkpoint_stage(self.query_request_id, request)? {
            GuardianCheckpointStageReplyV1::Acked {
                upload_id,
                completion_id,
                checkpoint_id,
                boundary_id,
                total_bytes,
            } if upload_id == self.upload_id
                && completion_id == self.completion_id
                && checkpoint_id == self.descriptor.checkpoint_id()
                && boundary_id == self.descriptor.boundary_id()
                && total_bytes == self.descriptor.total_bytes() =>
            {
                Ok(())
            }
            GuardianCheckpointStageReplyV1::Quarantined { .. } => {
                Err(GuardianProxyError::CheckpointStageQuarantined)
            }
            _ => Err(GuardianProxyError::CheckpointStageInvariant),
        }
    }
}

struct GuardianCheckpointPublisherState {
    transport: Box<dyn GuardianCheckpointStageTransport>,
    pending: Option<PendingGuardianCheckpointPublication>,
    last_acknowledged: Option<AcknowledgedGuardianCheckpointPublication>,
}

/// Serialized mux-side checkpoint publisher for one exact guardian lease.
///
/// A pending publication retains every request identity and the zeroizing
/// canonical terminal payload until Stage, mutation-sequenced adoption, and
/// durable Ack all reconcile. Dropping a reply can therefore never mint a new
/// upload or silently advance the actor's mutation sequence.
pub struct GuardianCheckpointPublisher {
    identity: GuardianPaneLeaseIdentity,
    actor: SharedGuardianPaneLeaseActor,
    state: Mutex<GuardianCheckpointPublisherState>,
}

impl fmt::Debug for GuardianCheckpointPublisher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointPublisher")
            .field("identity", &self.identity)
            .field(
                "has_pending_publication",
                &self.state.lock().pending.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointPublisher {
    fn connect(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
        actor: SharedGuardianPaneLeaseActor,
    ) -> Result<Self, GuardianProxyError> {
        if actor.lock().identity() != identity {
            return Err(GuardianProxyError::LeaseIdentityMismatch);
        }
        let transport =
            GuardianCheckpointStageClientTransport::connect(socket_path, token_path, identity)?;
        Ok(Self::with_transport(identity, actor, Box::new(transport)))
    }

    fn with_transport(
        identity: GuardianPaneLeaseIdentity,
        actor: SharedGuardianPaneLeaseActor,
        transport: Box<dyn GuardianCheckpointStageTransport>,
    ) -> Self {
        Self {
            identity,
            actor,
            state: Mutex::new(GuardianCheckpointPublisherState {
                transport,
                pending: None,
                last_acknowledged: None,
            }),
        }
    }

    fn drive_pending(
        actor: &SharedGuardianPaneLeaseActor,
        transport: &mut dyn GuardianCheckpointStageTransport,
        pending: &mut PendingGuardianCheckpointPublication,
    ) -> Result<GuardianCheckpointReceipt, GuardianProxyError> {
        let completion_id = pending.drive_to_sealed(transport)?;
        let adoption_receipt = if let Some(receipt) = pending.adoption_receipt {
            receipt
        } else {
            let receipt = actor.lock().publish_checkpoint(pending.intent())?;
            pending.adoption_receipt = Some(receipt);
            receipt
        };
        pending.drive_ack(transport, completion_id)?;
        Ok(adoption_receipt)
    }

    /// Publish one exact live-parser capture through Stage, catalog adoption,
    /// and durable Ack. Any earlier ambiguous publication is reconciled first.
    pub fn publish(
        &self,
        capture: LiveParserCheckpointAck,
    ) -> Result<PublishedGuardianCheckpoint, GuardianProxyError> {
        let mut state = self.state.lock();
        if state.pending.is_some() {
            let GuardianCheckpointPublisherState {
                transport,
                pending,
                last_acknowledged,
            } = &mut *state;
            let existing = pending
                .as_mut()
                .ok_or(GuardianProxyError::CheckpointStageInvariant)?;
            let receipt = Self::drive_pending(&self.actor, transport.as_mut(), existing)?;
            *last_acknowledged = Some(AcknowledgedGuardianCheckpointPublication::from_pending(
                existing, receipt,
            )?);
            *pending = None;
        }
        // Checkpoint identities describe content, not capture attempts. A
        // quiescent pane can be sampled repeatedly at the same durable output
        // boundary. Starting another upload would try to adopt that immutable
        // catalog identity under a different effect and correctly be rejected.
        // Rebind the fresh affine parser capture only to a receipt whose Ack
        // this exact publisher has already completed for its unchanged lease.
        if let Some(acknowledged) = state.last_acknowledged {
            let descriptor = GuardianCheckpointDescriptorV1::from_live_capture(
                &capture,
                self.identity.generation(),
            )
            .map_err(GuardianProxyError::ReplayProtocol)?;
            if acknowledged.receipt.intent()
                == GuardianCheckpointIntent::new(
                    descriptor.checkpoint_id(),
                    descriptor.boundary_id(),
                )
            {
                let mut actor = self.actor.lock();
                actor.ensure_identity(self.identity)?;
                if actor.pending.is_some() {
                    if let Some(RecoveredPendingMutation::InputApplied {
                        applied_bytes,
                        input_bytes,
                    }) = actor.reconcile_before_new_operation(None)?
                    {
                        if applied_bytes != input_bytes {
                            return Err(GuardianProxyError::PreviousInputPartiallyApplied {
                                applied_bytes,
                                input_bytes,
                            });
                        }
                    }
                }
                actor.ensure_attached()?;
                // A local Attached flag alone cannot detect an externally
                // fenced lease or damaged durable Ack. Revalidate through the
                // authenticated Stage query without creating another upload.
                acknowledged.confirm_ack(state.transport.as_mut())?;
                return capture
                    .bind_published(acknowledged.receipt, self.identity)
                    .map_err(|_| GuardianProxyError::CheckpointStageInvariant);
            }
        }
        let pending =
            PendingGuardianCheckpointPublication::from_live_capture(self.identity, capture)?;
        state.pending = Some(pending);
        let GuardianCheckpointPublisherState {
            transport,
            pending,
            last_acknowledged,
        } = &mut *state;
        let current = pending
            .as_mut()
            .ok_or(GuardianProxyError::CheckpointStageInvariant)?;
        let receipt = Self::drive_pending(&self.actor, transport.as_mut(), current)?;
        let acknowledged =
            AcknowledgedGuardianCheckpointPublication::from_pending(current, receipt)?;
        let completed = pending
            .take()
            .ok_or(GuardianProxyError::CheckpointStageInvariant)?;
        let published = completed
            .capture
            .bind_published(receipt, self.identity)
            .map_err(|_| GuardianProxyError::CheckpointStageInvariant)?;
        *last_acknowledged = Some(acknowledged);
        Ok(published)
    }
}

impl GuardianLiveCheckpointPublisher for GuardianCheckpointPublisher {
    fn publish_checkpoint(
        &self,
        capture: LiveParserCheckpointAck,
    ) -> anyhow::Result<PublishedGuardianCheckpoint> {
        self.publish(capture).map_err(anyhow::Error::new)
    }
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
    fn from_claimed_lease(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
        claimed_lease: GuardianClaimedPaneLease,
    ) -> Self {
        debug_assert_eq!(
            claimed_lease.guardian_incarnation(),
            identity.guardian_incarnation()
        );
        debug_assert_eq!(claimed_lease.mux_incarnation(), identity.mux_incarnation());
        debug_assert_eq!(claimed_lease.pane_id(), identity.pane_id());
        debug_assert_eq!(claimed_lease.generation(), identity.generation());
        Self {
            socket_path: socket_path.to_path_buf(),
            token_path: token_path.to_path_buf(),
            identity,
            client: Some(claimed_lease.into_client()),
        }
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

    fn checkpoint(
        &mut self,
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        request_id: Uuid,
        effect_id: Uuid,
        intent: GuardianCheckpointIntent,
    ) -> Result<GuardianCheckpointReceipt, GuardianMutationTransportError> {
        self.call(|client| {
            client.checkpoint(pane_id, generation, sequence, request_id, effect_id, intent)
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
    fn with_validated_transport(
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        transport: Box<dyn GuardianMutationTransport>,
    ) -> Self {
        debug_assert_ne!(next_sequence, 0);
        debug_assert!(validate_pty_size(size).is_ok());
        Self {
            identity,
            next_sequence,
            size,
            disposition: GuardianLeaseDisposition::Attached,
            pending: None,
            transport,
            #[cfg(test)]
            fail_next_input_copy: false,
        }
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

    fn begin_checkpoint(
        &mut self,
        intent: GuardianCheckpointIntent,
    ) -> Result<(), GuardianProxyError> {
        self.ensure_attached()?;
        if self.pending.is_some() {
            return Err(GuardianProxyError::UnexpectedMutationReply);
        }
        self.pending = Some(PendingMutation::Checkpoint(PendingCheckpointMutation {
            sequence: self.next_sequence,
            request_id: Uuid::new_v4(),
            effect_id: Uuid::new_v4(),
            intent,
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
            Some(PendingMutation::Checkpoint(pending)) => self.retry_checkpoint(pending),
            None => Err(GuardianProxyError::UnexpectedMutationReply),
        }
    }

    fn retry_checkpoint(
        &mut self,
        pending: PendingCheckpointMutation,
    ) -> Result<RecoveredPendingMutation, GuardianProxyError> {
        let result = self.transport.checkpoint(
            self.identity.pane_id(),
            self.identity.generation(),
            pending.sequence,
            pending.request_id,
            pending.effect_id,
            pending.intent,
        );
        match result {
            Ok(receipt)
                if receipt.pane_id() == self.identity.pane_id()
                    && receipt.generation() == self.identity.generation()
                    && receipt.sequence() == pending.sequence
                    && receipt.effect_id() == pending.effect_id
                    && receipt.intent() == pending.intent
                    && receipt.disposition() == GuardianCheckpointDisposition::Committed =>
            {
                self.complete_sequence(pending.sequence)?;
                Ok(RecoveredPendingMutation::Checkpoint(receipt))
            }
            Ok(receipt)
                if receipt.pane_id() == self.identity.pane_id()
                    && receipt.generation() == self.identity.generation()
                    && receipt.sequence() == pending.sequence
                    && receipt.effect_id() == pending.effect_id
                    && receipt.intent() == pending.intent
                    && receipt.disposition()
                        == GuardianCheckpointDisposition::OutcomeIndeterminate =>
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
            Some(PendingMutation::Generic(_) | PendingMutation::Checkpoint(_)) | None => {
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
            Some(
                PendingMutation::Input(_)
                | PendingMutation::Generic(_)
                | PendingMutation::Checkpoint(_),
            )
            | None => {
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
                    | RecoveredPendingMutation::Checkpoint(_)
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
            RecoveredPendingMutation::Generic | RecoveredPendingMutation::Checkpoint(_) => {
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
            RecoveredPendingMutation::Generic | RecoveredPendingMutation::Checkpoint(_) => Ok(()),
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
                    | RecoveredPendingMutation::Checkpoint(_)
                    | RecoveredPendingMutation::InputApplied { .. }
                    | RecoveredPendingMutation::InputKnownNotApplied,
                )
                | None => {}
            }
        }
        if (kind == GenericMutation::Close && self.disposition == GuardianLeaseDisposition::Closed)
            || (kind == GenericMutation::Retire
                && matches!(
                    self.disposition,
                    GuardianLeaseDisposition::Closed | GuardianLeaseDisposition::Retired
                ))
        {
            return Ok(());
        }
        self.begin_generic(kind)?;
        match self.retry_pending(None)? {
            RecoveredPendingMutation::Generic => Ok(()),
            RecoveredPendingMutation::Checkpoint(_)
            | RecoveredPendingMutation::InputApplied { .. }
            | RecoveredPendingMutation::InputKnownNotApplied => {
                self.disposition = GuardianLeaseDisposition::Quarantined;
                Err(GuardianProxyError::UnexpectedMutationReply)
            }
        }
    }

    fn publish_checkpoint(
        &mut self,
        intent: GuardianCheckpointIntent,
    ) -> Result<GuardianCheckpointReceipt, GuardianProxyError> {
        if self.pending.is_some() {
            let same_checkpoint = self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.matches_checkpoint(intent));
            let recovered = self.reconcile_before_new_operation(None)?;
            match recovered {
                Some(RecoveredPendingMutation::Checkpoint(receipt)) if same_checkpoint => {
                    return Ok(receipt);
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
                Some(
                    RecoveredPendingMutation::Generic
                    | RecoveredPendingMutation::Checkpoint(_)
                    | RecoveredPendingMutation::InputApplied { .. }
                    | RecoveredPendingMutation::InputKnownNotApplied,
                )
                | None => {}
            }
        }
        self.begin_checkpoint(intent)?;
        match self.retry_pending(None)? {
            RecoveredPendingMutation::Checkpoint(receipt) => Ok(receipt),
            RecoveredPendingMutation::Generic
            | RecoveredPendingMutation::InputApplied { .. }
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

    fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
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

struct ValidatedReplayPageIdentity(GuardianPaneLeaseIdentity);

fn validate_replay_page_identity(
    page: &GuardianReplayPageDelivery,
    identity: GuardianPaneLeaseIdentity,
) -> Result<ValidatedReplayPageIdentity, GuardianProxyError> {
    if page.header().pane_id() != identity.pane_id()
        || page.header().generation() != identity.generation()
    {
        Err(GuardianProxyError::LeaseIdentityMismatch)
    } else {
        Ok(ValidatedReplayPageIdentity(identity))
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
        if let GuardianCheckpointOutputBoundaryV1::Genesis {
            parser_stream_bytes,
            ..
        } = descriptor.output_boundary()
        {
            if parser_stream_bytes != 0
                || descriptor.capture_generation()
                    != mux::guardian_protocol::GUARDIAN_GENESIS_CAPTURE_GENERATION
                || descriptor.durable_pane_id().is_some()
            {
                return Err(GuardianProxyError::ReplayInvariant(
                    "genesis replay does not begin at the canonical zero origin",
                ));
            }
            descriptor
                .canonical_descriptor()
                .map_err(GuardianProxyError::ReplayProtocol)?;
            return Ok(Self {
                next_sequence: 1,
                previous_record_digest: [0; 32],
                cumulative_plaintext_bytes: 0,
            });
        }
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

struct VerifiedGuardianReplayRestore {
    inert_terminal: GuardianRestoredTerminal,
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
    page_identity: ValidatedReplayPageIdentity,
    expected_size: PtySize,
    limits: TerminalCheckpointLimits,
) -> Result<(), GuardianProxyError> {
    let identity = page_identity.0;
    // Genesis has no pane until the guardian publishes/adopts its Spawn.
    // Its association is supplied only by the authenticated replay page,
    // after the guardian reconciles the committed catalog and initial journal.
    if matches!(
        descriptor.output_boundary(),
        GuardianCheckpointOutputBoundaryV1::Record { .. }
    ) && descriptor.durable_pane_id() != Some(identity.pane_id())
    {
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
    pane_id: Uuid,
    descriptor: GuardianCheckpointDescriptorV1,
    checkpoint: &BoundedReplayBuffer,
    expected_size: PtySize,
    config: Arc<dyn TerminalConfiguration>,
    limits: TerminalCheckpointLimits,
) -> Result<GuardianRestoredTerminal, GuardianProxyError> {
    if u64::try_from(checkpoint.len()) != Ok(descriptor.total_bytes()) {
        return Err(GuardianProxyError::ReplayInvariant(
            "checkpoint replay did not assemble its exact declared length",
        ));
    }
    GuardianRestoredTerminal::from_checkpoint(
        pane_id,
        descriptor,
        checkpoint.as_slice(),
        (
            u64::from(expected_size.pixel_width),
            u64::from(expected_size.pixel_height),
        ),
        config,
        limits,
    )
    .map_err(GuardianProxyError::RestoredModel)
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
    selected: Option<SelectedRecoveryCheckpoint>,
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
        selector: selected.map_or(GuardianReplaySelectorV1::LatestCompatible, |checkpoint| {
            GuardianReplaySelectorV1::ExactCheckpoint {
                checkpoint_id: checkpoint.checkpoint_id,
            }
        }),
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
        let page_identity = validate_replay_page_identity(&page, identity)?;
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
                if let Some(selected) = selected {
                    selected.validate_descriptor(observed_descriptor)?;
                }
                validate_checkpoint_descriptor_for_proxy(
                    observed_descriptor,
                    page_identity,
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
                        identity.pane_id(),
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
                    let observed = restored
                        .replay_record(record)
                        .map_err(GuardianProxyError::RestoredModel)?;
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
    selected: Option<SelectedRecoveryCheckpoint>,
) -> Result<VerifiedGuardianReplayRestore, GuardianProxyError> {
    for attempt in 0..GUARDIAN_RESTORE_REOPEN_ATTEMPTS {
        match consume_one_guardian_replay_snapshot(
            transport,
            identity,
            expected_size,
            Arc::clone(&config),
            limits,
            selected,
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
    transport_retry_pending: bool,
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
            .field("transport_retry_pending", &self.transport_retry_pending)
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
            transport_retry_pending: false,
            maximum_record_bytes,
            maximum_page_records,
            idle_poll_interval: GUARDIAN_REPLAY_IDLE_POLL_MIN_INTERVAL,
        })
    }

    fn retain_transport_retry(&mut self, error: GuardianProxyError) -> io::Error {
        self.transport_retry_pending = !self.delivery_failed
            && (self.pending_replay.is_some() || self.pending_ack.is_some())
            && replay_error_is_retryable_io(&error);
        error.into()
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
    fn has_pending_transport_retry(&self) -> bool {
        self.transport_retry_pending
    }

    fn deliver_next_record(
        &mut self,
        deliver: &mut dyn FnMut(
            mux::guardian_output_journal::GuardianOutputSegmentIdentity,
            mux::guardian_output_journal::GuardianOutputAppendReceipt,
            Arc<[u8]>,
        ) -> io::Result<()>,
    ) -> io::Result<GuardianLiveOutputDelivery> {
        self.transport_retry_pending = false;
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
                let completed_replay_page = self.records.is_empty();
                if completed_replay_page {
                    if self.pending_boundary != Some(candidate) {
                        self.delivery_failed = true;
                        return Err(io::Error::from(GuardianProxyError::ReplayInvariant(
                            "delivered replay records omitted their terminal page boundary",
                        )));
                    }
                    self.finish_delivered_page()
                        .map_err(|error| self.retain_transport_retry(error))?;
                }
                return Ok(if completed_replay_page {
                    GuardianLiveOutputDelivery::replay_page_acknowledged()
                } else {
                    GuardianLiveOutputDelivery::buffered_within_replay_page()
                });
            }
            self.finish_delivered_page()
                .map_err(|error| self.retain_transport_retry(error))?;
            if let Some(error) = self.pending_terminal_error.take() {
                return Err(io::Error::from(error.into_proxy_error()));
            }
            self.load_next_output_page()
                .map_err(|error| self.retain_transport_retry(error))?;
        }
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
            GuardianReplayReaderState::Ready(None) => {
                *state = prior;
                Err(GuardianProxyError::InvalidConfiguration(
                    "guardian replay reader is unavailable before exact restore activation or after its single take",
                ))
            }
            GuardianReplayReaderState::Staged | GuardianReplayReaderState::Taken => {
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

/// An exact checkpoint selected from an authenticated whole-mux image.
/// Only `prepare_from_recovery` mints this authority in production; neither
/// caller-supplied provenance nor a digest alone authorizes replay selection.
#[derive(Clone, Copy)]
struct SelectedRecoveryCheckpoint {
    pane_id: Uuid,
    generation: u64,
    checkpoint_id: GuardianCheckpointIdentityDigest,
    boundary_id: [u8; 32],
    payload_digest: [u8; 32],
    payload_bytes: u64,
}

impl SelectedRecoveryCheckpoint {
    fn validate_descriptor(
        self,
        descriptor: GuardianCheckpointDescriptorV1,
    ) -> Result<(), GuardianProxyError> {
        if descriptor.checkpoint_id() != self.checkpoint_id
            || descriptor.boundary_id().into_bytes() != self.boundary_id
            || descriptor.durable_pane_id() != Some(self.pane_id)
            || descriptor.capture_generation() != self.generation
            || descriptor.terminal_payload_digest() != self.payload_digest
            || descriptor.total_bytes() != self.payload_bytes
        {
            return Err(GuardianProxyError::ReplayInvariant(
                "guardian replay differs from the selected whole-mux checkpoint",
            ));
        }
        Ok(())
    }

    fn validate_attach(self, pane_id: Uuid, generation: u64) -> Result<(), GuardianProxyError> {
        if pane_id != self.pane_id || self.generation.checked_add(1) != Some(generation) {
            return Err(GuardianProxyError::InvalidConfiguration(
                "recovery attachment differs from the selected pane and successor generation",
            ));
        }
        Ok(())
    }
}

/// Pre-Claim validation and transport authority for one guardian proxy lease.
///
/// Construction validates the PTY configuration and authenticates a client
/// against the exact shared census coordinator before any lease mutation is
/// sent. Consuming [`Self::claim`] or [`Self::attach`] is therefore the only
/// production path into [`GuardianProxyStaging`]: once the guardian grants the
/// lease, staging can install its rollback guard without another fallible
/// configuration or identity check.
pub struct GuardianProxyLeasePlan {
    socket_path: PathBuf,
    token_path: PathBuf,
    size: PtySize,
    census: Arc<GuardianCensusCoordinator>,
    client: GuardianClient,
    spawn_custody: Option<GuardianSpawnCustodyScopeV1>,
    successor_custody: Option<mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1>,
    build_authenticated: bool,
    selected_checkpoint: Option<SelectedRecoveryCheckpoint>,
}

impl fmt::Debug for GuardianProxyLeasePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GuardianProxyLeasePlan")
            .field("guardian_incarnation", &self.client.guardian_incarnation())
            .field("mux_incarnation", &self.client.mux_incarnation())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl GuardianProxyLeasePlan {
    /// Prepare the exact next successor from validated image provenance.
    /// This only reopens existing custody; it neither claims nor publishes.
    pub fn prepare_from_recovery(
        socket_path: &Path,
        token_path: &Path,
        size: PtySize,
        census: Arc<GuardianCensusCoordinator>,
        recovery: &frankenterm_core::session_restore::ValidatedWholeMuxRecovery,
        durable_pane_id: Uuid,
    ) -> Result<Self, GuardianProxyError> {
        use frankenterm_core::mux_recovery_image::{CheckpointAuthority, RecoverySpawnCustody};
        let pane = recovery
            .image()
            .panes
            .iter()
            .find(|pane| Uuid::parse_str(&pane.pane_uuid).ok() == Some(durable_pane_id))
            .ok_or(GuardianProxyError::InvalidConfiguration(
                "selected recovery image has no matching pane",
            ))?;
        if u16::try_from(pane.size.rows) != Ok(size.rows)
            || u16::try_from(pane.size.cols) != Ok(size.cols)
            || u16::try_from(pane.size.pixel_width) != Ok(size.pixel_width)
            || u16::try_from(pane.size.pixel_height) != Ok(size.pixel_height)
        {
            return Err(GuardianProxyError::InvalidConfiguration(
                "recovery PTY geometry differs from the selected image",
            ));
        }
        let CheckpointAuthority::Guardian {
            guardian_generation,
            publication,
            ..
        } = &pane.checkpoint.authority
        else {
            return Err(GuardianProxyError::InvalidConfiguration(
                "model-only checkpoint cannot authorize a live guardian restore",
            ));
        };
        let payload = recovery
            .checkpoint_payload(&pane.checkpoint.checkpoint_ref.object_id)
            .ok_or(GuardianProxyError::InvalidConfiguration(
                "selected recovery checkpoint has no validated payload",
            ))?;
        let (payload_bytes, payload_digest) =
            mux::guardian_checkpoint::terminal_payload_identity(payload).map_err(|_| {
                GuardianProxyError::InvalidConfiguration(
                    "selected recovery checkpoint payload has no guardian identity",
                )
            })?;
        let selected = SelectedRecoveryCheckpoint {
            pane_id: durable_pane_id,
            generation: *guardian_generation,
            checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes(
                publication.checkpoint_identity,
            )
            .map_err(|_| {
                GuardianProxyError::InvalidConfiguration(
                    "selected recovery checkpoint identity is invalid",
                )
            })?,
            boundary_id: publication.output_boundary_identity,
            payload_digest,
            payload_bytes,
        };
        let RecoverySpawnCustody::Original {
            broker_lineage,
            guardian_incarnation,
            original_mux_incarnation,
            broker_build,
            guardian_build,
            original_mux_build,
            pane_id,
            spawn_effect_id,
            current_mux_incarnation,
            current_lease_generation,
            acknowledged_successor,
        } = &pane.spawn_custody
        else {
            return Err(GuardianProxyError::InvalidConfiguration(
                "legacy pane has no original guardian custody",
            ));
        };
        if *current_lease_generation != selected.generation
            // ubs:ignore[rust.security.constant-time-compare] — Pane UUIDs are public custody identities, not secret authentication material.
            || *pane_id != selected.pane_id
            || (*current_lease_generation == 1 && current_mux_incarnation != original_mux_incarnation)
            || *current_mux_incarnation == census.mux_incarnation()
        {
            return Err(GuardianProxyError::InvalidConfiguration(
                "selected custody differs from checkpoint generation or current owner",
            ));
        }
        let scope = GuardianSpawnCustodyScopeV1 {
            broker_lineage: *broker_lineage,
            guardian_incarnation: *guardian_incarnation,
            mux_incarnation: *original_mux_incarnation,
            broker_build: *broker_build,
            guardian_build: *guardian_build,
            mux_build: *original_mux_build,
            pane_id: *pane_id,
            effect_id: *spawn_effect_id,
        };
        let custody =
            GuardianDurableSpawnCustodyV1::open_existing(token_path, scope).map_err(|_| {
                GuardianProxyError::InvalidConfiguration(
                    "original guardian custody is missing or unauthenticated",
                )
            })?;
        let successor = if *current_lease_generation > 1 {
            let expected: mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1 =
                acknowledged_successor
                    .ok_or(GuardianProxyError::InvalidConfiguration(
                        "successor image has no acknowledged custody selector",
                    ))?
                    .into();
            if expected.broker_incarnation != custody.broker_incarnation() {
                return Err(GuardianProxyError::InvalidConfiguration(
                    "successor custody broker lineage mismatch",
                ));
            }
            let actual =
                frankenterm_pty_guardian::GuardianDurableSuccessorCustodyV1::open_existing(
                    token_path,
                    expected.scope(),
                )
                .map_err(|_| {
                    GuardianProxyError::InvalidConfiguration(
                        "successor custody is missing or unauthenticated",
                    )
                })?;
            if actual.context() != expected {
                return Err(GuardianProxyError::InvalidConfiguration(
                    "successor custody ACK or connection differs",
                ));
            }
            Some(expected)
        } else {
            None
        };
        let mut plan = Self::prepare_with_build(socket_path, token_path, size, census, true)?
            .with_spawn_custody(custody)?;
        plan.successor_custody = successor;
        plan.selected_checkpoint = Some(selected);
        Ok(plan)
    }

    /// Validate local proxy state and authenticate the exact pre-Claim client.
    pub fn prepare(
        socket_path: &Path,
        token_path: &Path,
        size: PtySize,
        census: Arc<GuardianCensusCoordinator>,
    ) -> Result<Self, GuardianProxyError> {
        Self::prepare_with_build(socket_path, token_path, size, census, false)
    }

    fn prepare_with_build(
        socket_path: &Path,
        token_path: &Path,
        size: PtySize,
        census: Arc<GuardianCensusCoordinator>,
        build_authenticated: bool,
    ) -> Result<Self, GuardianProxyError> {
        validate_pty_size(size)?;
        if let Err(error) = census.retry_retained_lease_cleanup() {
            log::warn!(
                "retained guardian lease retirement remains pending before a new lease plan: {error}"
            );
        }
        let client = if build_authenticated {
            GuardianClient::connect_for_genesis(socket_path, token_path, census.mux_incarnation())
        } else {
            GuardianClient::connect(socket_path, token_path, census.mux_incarnation())
        }
        .map_err(GuardianProxyError::Client)?;
        if client.guardian_incarnation() != census.guardian_incarnation() {
            return Err(GuardianProxyError::GuardianIncarnationChanged);
        }
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            token_path: token_path.to_path_buf(),
            size,
            census,
            client,
            spawn_custody: None,
            successor_custody: None,
            build_authenticated,
            selected_checkpoint: None,
        })
    }

    /// Bind independently authenticated existing custody to this unpublished
    /// plan. Image metadata alone cannot construct this capability.
    pub fn with_spawn_custody(
        mut self,
        custody: GuardianDurableSpawnCustodyV1,
    ) -> Result<Self, GuardianProxyError> {
        let scope = custody.scope();
        if scope.guardian_incarnation != self.client.guardian_incarnation() {
            return Err(GuardianProxyError::GuardianIncarnationChanged);
        }
        if scope.mux_incarnation != self.client.mux_incarnation() && !self.build_authenticated {
            self.client = GuardianClient::connect_for_genesis(
                &self.socket_path,
                &self.token_path,
                self.census.mux_incarnation(),
            )
            .map_err(GuardianProxyError::Client)?;
            if self.client.guardian_incarnation() != scope.guardian_incarnation {
                return Err(GuardianProxyError::GuardianIncarnationChanged);
            }
            self.build_authenticated = true;
        }
        self.spawn_custody = Some(scope);
        Ok(self)
    }

    /// Claim one currently unowned pane and immediately install rollback
    /// authority before opening any secondary replay or checkpoint channel.
    /// Transport failures retry the exact request under a finite attempt and
    /// retry-admission budget. Exhaustion retains unresolved Claim authority
    /// in the coordinator for later recovery and retirement.
    pub fn claim(
        self,
        pane_id: Uuid,
        observed_generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Result<GuardianProxyStaging, GuardianProxyError> {
        if let Some(scope) = self.spawn_custody {
            let initial = scope.mux_incarnation == self.client.mux_incarnation();
            let expected_generation = self
                .successor_custody
                .map_or(u64::from(!initial), |c| c.lease_generation);
            if scope.pane_id != pane_id || observed_generation != expected_generation {
                return Err(GuardianProxyError::InvalidConfiguration(
                    "custody does not match initial birth or first successor claim",
                ));
            }
        }
        if request_id.is_nil() || effect_id.is_nil() {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian Claim request and effect identities must be nonzero",
            ));
        }
        let generation = observed_generation
            .checked_add(1)
            .ok_or(GuardianProxyError::Client(GuardianClientError::Protocol(
                GuardianProtocolError::GenerationExhausted,
            )))?;
        if let Some(selected) = self.selected_checkpoint {
            selected.validate_attach(pane_id, generation)?;
        }
        let identity = GuardianPaneLeaseIdentity::new(
            self.client.guardian_incarnation(),
            self.client.mux_incarnation(),
            pane_id,
            generation,
        )
        .map_err(|_| {
            GuardianProxyError::InvalidConfiguration(
                "guardian Claim plan carried an invalid lease identity",
            )
        })?;
        let retirement_reservation = self.census.reserve_retirement(identity)?;
        let Self {
            socket_path,
            token_path,
            size,
            census,
            client,
            spawn_custody,
            successor_custody,
            build_authenticated: _,
            selected_checkpoint,
        } = self;
        let pending = Box::new(GuardianPendingClaim {
            socket_path: socket_path.clone(),
            token_path: token_path.clone(),
            identity,
            observed_generation,
            request_id,
            effect_id,
            size,
            spawn_custody,
            successor_custody,
        });
        let started = Instant::now();
        let mut attempts = 0_u32;
        let mut ambiguous = false;
        let mut client = Some(client);
        let claimed_lease = loop {
            let mut definitive_rejection = false;
            let connection = client.take().map_or_else(|| pending.connect(), Ok);
            let result = connection.and_then(|client| {
                pending.claim(client).map_err(|error| {
                    definitive_rejection = matches!(
                        &error,
                        GuardianClientError::Rejected(_) | GuardianClientError::Setup(_)
                    );
                    map_replay_client_error(error)
                })
            });
            match result {
                Ok(lease) => break lease,
                Err(error) => {
                    attempts += 1;
                    let retryable = matches!(
                        &error,
                        GuardianProxyError::Client(GuardianClientError::Io(_))
                    );
                    // An invalid reply can follow an applied Claim. Only an
                    // authenticated rejection establishes a definite outcome
                    // before any earlier ambiguous attempt.
                    ambiguous |= !definitive_rejection;
                    if retryable && attempts < 32 && started.elapsed() < Duration::from_secs(5) {
                        std::thread::sleep(Duration::from_millis(10));
                        if started.elapsed() < Duration::from_secs(5) {
                            continue;
                        }
                    }
                    // A Claim may have applied before any transport failure.
                    // Preserve the original request even if a later reconnect
                    // reports a terminal authority error. No lease is invented.
                    if ambiguous {
                        retirement_reservation.retain_authority(
                            GuardianCleanupAuthority::PendingClaim(pending),
                            !retryable,
                        );
                    }
                    return Err(error);
                }
            }
        };
        let acknowledged = claimed_lease.acknowledged_successor_claim();
        let mut staging = GuardianProxyStaging::from_planned_lease(
            &socket_path,
            &token_path,
            identity,
            claimed_lease,
            retirement_reservation,
            size,
            census,
        )?;
        staging.spawn_custody = spawn_custody;
        if let Some(acknowledged) = acknowledged {
            staging.successor_custody = Some(
                acknowledged
                    .reopen(
                        &token_path,
                        spawn_custody.ok_or(GuardianProxyError::InvalidConfiguration(
                            "successor requires original lineage",
                        ))?,
                    )
                    .map_err(map_replay_client_error)?,
            );
        }
        staging.selected_checkpoint = selected_checkpoint;
        Ok(staging)
    }

    /// Attach to one lease already owned by this mux and immediately install
    /// rollback authority before opening secondary transport channels.
    pub fn attach(
        self,
        pane_id: Uuid,
        generation: u64,
        request_id: Uuid,
    ) -> Result<GuardianProxyStaging, GuardianProxyError> {
        if let Some(selected) = self.selected_checkpoint {
            selected.validate_attach(pane_id, generation)?;
        }
        let identity = GuardianPaneLeaseIdentity::new(
            self.client.guardian_incarnation(),
            self.client.mux_incarnation(),
            pane_id,
            generation,
        )
        .map_err(|_| {
            GuardianProxyError::InvalidConfiguration(
                "guardian Attach plan carried an invalid lease identity",
            )
        })?;
        let retirement_reservation = self.census.reserve_retirement(identity)?;
        let Self {
            socket_path,
            token_path,
            size,
            census,
            client,
            spawn_custody,
            successor_custody,
            build_authenticated: _,
            selected_checkpoint,
        } = self;
        let claimed_lease = client
            .attach(pane_id, generation, request_id)
            .map_err(map_replay_client_error)?;
        let mut staging = GuardianProxyStaging::from_planned_lease(
            &socket_path,
            &token_path,
            identity,
            claimed_lease,
            retirement_reservation,
            size,
            census,
        )?;
        staging.spawn_custody = spawn_custody;
        staging.successor_custody = successor_custody;
        staging.selected_checkpoint = selected_checkpoint;
        Ok(staging)
    }
}

/// A connected, already-claimed guardian lease that is still off topology.
///
/// No portable-pty object and no reader activation method are exposed from this
/// type. The consuming restore transaction must bind terminal restoration to
/// its authenticated final sequence/digest and retain a record-aware live
/// reader before any caller can construct a pane.
pub struct GuardianProxyStaging {
    spawn_custody: Option<GuardianSpawnCustodyScopeV1>,
    successor_custody: Option<mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1>,
    selected_checkpoint: Option<SelectedRecoveryCheckpoint>,
    actor: SharedGuardianPaneLeaseActor,
    census: Arc<GuardianCensusCoordinator>,
    reader_slot: Arc<GuardianReplayReaderSlot>,
    replay_transport: Option<Box<dyn GuardianReplayTransport>>,
    checkpoint_publisher: Option<Arc<GuardianCheckpointPublisher>>,
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
            .field(
                "has_checkpoint_publisher",
                &self.checkpoint_publisher.is_some(),
            )
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
    retirement_reservation: Option<GuardianRetirementReservation>,
    armed: bool,
}

impl GuardianClaimedLeaseRollback {
    fn new(
        actor: SharedGuardianPaneLeaseActor,
        retirement_reservation: GuardianRetirementReservation,
    ) -> Self {
        Self {
            actor,
            retirement_reservation: Some(retirement_reservation),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        if let Some(mut reservation) = self.retirement_reservation.take() {
            reservation.release();
        }
        self.armed = false;
    }

    fn retain_for_later_retry(&mut self, error: &GuardianProxyError) {
        log::error!(
            "guardian lease retirement after unpublished restore remains unconfirmed; retaining exact cleanup authority: {error}"
        );
        if let Some(reservation) = self.retirement_reservation.take() {
            reservation.retain(
                Arc::clone(&self.actor),
                !guardian_cleanup_retry_is_safe(error),
            );
        }
        self.armed = false;
    }

    fn resolve_absent_lease(&mut self, error: &GuardianProxyError) {
        log::warn!(
            "guardian lease retirement after unpublished restore is already resolved by terminal lease state: {error}"
        );
        metrics::counter!(
            "mux.guardian_proxy.retained_retirement_retry_total",
            "outcome" => "already_absent",
        )
        .increment(1);
        self.disarm();
    }

    fn retire_unpublished_lease(&mut self) {
        if !self.armed {
            return;
        }
        let Some(reservation) = self.retirement_reservation.as_ref() else {
            self.armed = false;
            return;
        };
        let identity = reservation.identity;
        reservation.coordinator.invalidate();
        let first_result = self.actor.lock().retire(identity);
        match first_result {
            Ok(()) => self.disarm(),
            Err(error) if guardian_retirement_is_resolved(&error) => {
                self.resolve_absent_lease(&error);
            }
            Err(error) if guardian_cleanup_retry_is_safe(&error) => {
                log::warn!(
                    "guardian lease retirement reply was lost before topology publication; retrying the exact pending mutation: {error}"
                );
                let retry_result = self.actor.lock().retire(identity);
                match retry_result {
                    Ok(()) => self.disarm(),
                    Err(retry_error) if guardian_retirement_is_resolved(&retry_error) => {
                        self.resolve_absent_lease(&retry_error);
                    }
                    Err(retry_error) => self.retain_for_later_retry(&retry_error),
                }
            }
            Err(error) => self.retain_for_later_retry(&error),
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

fn guardian_retirement_is_resolved(error: &GuardianProxyError) -> bool {
    matches!(
        error,
        GuardianProxyError::LeaseFenced
            | GuardianProxyError::PaneNotFound
            | GuardianProxyError::GuardianIncarnationChanged
    )
}

impl GuardianProxyStaging {
    fn from_planned_lease(
        socket_path: &Path,
        token_path: &Path,
        identity: GuardianPaneLeaseIdentity,
        claimed_lease: GuardianClaimedPaneLease,
        retirement_reservation: GuardianRetirementReservation,
        size: PtySize,
        census: Arc<GuardianCensusCoordinator>,
    ) -> Result<Self, GuardianProxyError> {
        debug_assert_eq!(
            claimed_lease.guardian_incarnation(),
            identity.guardian_incarnation()
        );
        debug_assert_eq!(claimed_lease.mux_incarnation(), identity.mux_incarnation());
        debug_assert_eq!(claimed_lease.pane_id(), identity.pane_id());
        debug_assert_eq!(claimed_lease.generation(), identity.generation());
        let next_sequence = claimed_lease.next_sequence();
        let mutation_transport = GuardianClientTransport::from_claimed_lease(
            socket_path,
            token_path,
            identity,
            claimed_lease,
        );
        let mut staging = Self::with_validated_transports(
            identity,
            next_sequence,
            size,
            Box::new(mutation_transport),
            census,
            retirement_reservation,
        );
        // Stage the rollback guard before opening the independent replay
        // channel. A replay-channel setup failure must not leak the Claim that
        // the caller completed before entering this constructor.
        let replay_transport =
            GuardianReplayClientTransport::connect(socket_path, token_path, identity)?;
        staging.replay_transport = Some(Box::new(replay_transport));
        let checkpoint_publisher = GuardianCheckpointPublisher::connect(
            socket_path,
            token_path,
            identity,
            Arc::clone(&staging.actor),
        )?;
        staging.checkpoint_publisher = Some(Arc::new(checkpoint_publisher));
        Ok(staging)
    }

    #[cfg(test)]
    fn with_transports(
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        transport: Box<dyn GuardianMutationTransport>,
        census: Arc<GuardianCensusCoordinator>,
    ) -> Result<Self, GuardianProxyError> {
        census.ensure_binding(identity)?;
        if next_sequence == 0 {
            return Err(GuardianProxyError::InvalidConfiguration(
                "next mutation sequence must be nonzero",
            ));
        }
        validate_pty_size(size)?;
        let retirement_reservation = census.reserve_retirement(identity)?;
        Ok(Self::with_validated_transports(
            identity,
            next_sequence,
            size,
            transport,
            census,
            retirement_reservation,
        ))
    }

    fn with_validated_transports(
        identity: GuardianPaneLeaseIdentity,
        next_sequence: u64,
        size: PtySize,
        transport: Box<dyn GuardianMutationTransport>,
        census: Arc<GuardianCensusCoordinator>,
        retirement_reservation: GuardianRetirementReservation,
    ) -> Self {
        debug_assert!(census.ensure_binding(identity).is_ok());
        let actor = Arc::new(Mutex::new(
            GuardianPaneLeaseActor::with_validated_transport(
                identity,
                next_sequence,
                size,
                transport,
            ),
        ));
        // Claim/Attach completed before staging construction and may postdate
        // an otherwise fresh shared snapshot. Never publish a new pane
        // against a cache that could still describe the pre-claim fleet.
        census.invalidate();
        let lease_rollback =
            GuardianClaimedLeaseRollback::new(Arc::clone(&actor), retirement_reservation);
        #[cfg(test)]
        let checkpoint_publisher = Some(Arc::new(GuardianCheckpointPublisher::with_transport(
            identity,
            Arc::clone(&actor),
            Box::new(DormantCheckpointStageTransport),
        )));
        #[cfg(not(test))]
        let checkpoint_publisher = None;
        Self {
            actor,
            census,
            reader_slot: Arc::new(GuardianReplayReaderSlot::new()),
            spawn_custody: None,
            successor_custody: None,
            selected_checkpoint: None,
            replay_transport: None,
            checkpoint_publisher,
            lease_rollback,
        }
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
        if self.checkpoint_publisher.is_none() {
            return Err(GuardianProxyError::InvalidConfiguration(
                "guardian staging has no checkpoint publisher bound to its claimed lease",
            ));
        }
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
            self.selected_checkpoint,
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
        inert_terminal: GuardianRestoredTerminal,
        guardian_live_output_reader: Box<dyn GuardianLiveOutputReader>,
    ) -> Result<ActivatedGuardianProxy, GuardianProxyError> {
        let identity = self.identity();
        let terminal_writer = GuardianProxyWriter {
            actor: Arc::clone(&self.actor),
        };
        let (terminal, restored_prefix) = inert_terminal
            .activate(Box::new(terminal_writer.clone()))
            .map_err(GuardianProxyError::RestoredModel)?;
        let actor = Arc::clone(&self.actor);
        Ok(ActivatedGuardianProxy {
            spawn_custody: self.spawn_custody,
            successor_custody: self.successor_custody,
            terminal,
            restored_prefix: Some(restored_prefix),
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
            guardian_checkpoint_publisher: self.checkpoint_publisher,
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
            spawn_custody: self.spawn_custody,
            successor_custody: self.successor_custody,
            terminal,
            restored_prefix: None,
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
            guardian_checkpoint_publisher: self.checkpoint_publisher,
            lease_identity: identity,
            lease_rollback: self.lease_rollback,
        }
    }
}

/// Fully restored guardian proxy facets that remain unpublished until the
/// caller deliberately constructs and registers a [`LocalPane`].
pub struct ActivatedGuardianProxy {
    spawn_custody: Option<GuardianSpawnCustodyScopeV1>,
    successor_custody: Option<mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1>,
    terminal: Terminal,
    restored_prefix: Option<GuardianRestoredParserPrefix>,
    process: Box<dyn Child + Send>,
    pty: Box<dyn MasterPty>,
    writer: Box<dyn Write + Send>,
    lease_control: Arc<dyn GuardianPaneLeaseControl>,
    guardian_live_output_reader: Option<Box<dyn GuardianLiveOutputReader>>,
    guardian_checkpoint_publisher: Option<Arc<GuardianCheckpointPublisher>>,
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
            self.guardian_checkpoint_publisher
                .take()
                .expect("verified guardian activation must retain its checkpoint publisher"),
            self.spawn_custody.map(|original| {
                mux::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1 {
                    original,
                    current_mux_incarnation: self.lease_identity.mux_incarnation(),
                    current_lease_generation: self.lease_identity.generation(),
                    acknowledged_successor: self.successor_custody,
                }
            }),
            self.restored_prefix,
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
        let mut observation_unavailable = false;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => observation_unavailable = false,
                Err(error) => {
                    // Checkpoint/replay workers temporarily own the guardian's
                    // protocol state. Census then closes retryably; this is not
                    // evidence that the broker-owned child exited. Returning
                    // that error to LocalPane's waiter would schedule exit
                    // pruning and explicitly close the still-running child.
                    // transport_failure fences/quarantines permanent failures;
                    // only an unchanged attached lease permits another query.
                    if self.actor.lock().disposition != GuardianLeaseDisposition::Attached {
                        return Err(error);
                    }
                    metrics::counter!("mux.guardian_proxy.child_observation_retry_total")
                        .increment(1);
                    if !observation_unavailable {
                        log::warn!(
                            "guardian child observation temporarily unavailable; retaining live child and retrying: {error}"
                        );
                        observation_unavailable = true;
                    }
                }
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
        GuardianCheckpointChunkDelivery, GuardianCheckpointStageKindV1,
        GuardianReplayOutputRecordsDelivery, GuardianReplayPhaseV1, GuardianReplayRecordDelivery,
        GuardianReplayRecordMetadataV1,
    };
    use mux::pane::{Pane, alloc_pane_id};
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use wezterm_term::color::ColorPalette;
    use wezterm_term::terminalstate::checkpoint::{TerminalCheckpointLimits, TerminalCheckpointV2};
    use wezterm_term::{InertTerminal, Terminal, TerminalConfiguration, TerminalSize};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RealBirthFixtureMode {
        BirthAndCancellation,
        InFlightCancellation,
        SuccessorImageRecovery,
    }

    #[test]
    #[ignore = "requires actual sealed candidate identity; run explicitly in strict RCH/DSR"]
    fn guardian_domain_real_birth_publishes_output_and_cancellation_retires_only_lease() {
        run_guardian_domain_real_birth(RealBirthFixtureMode::BirthAndCancellation);
    }

    #[test]
    #[ignore = "requires actual sealed candidate identity; run explicitly in strict RCH/DSR"]
    fn guardian_domain_in_flight_cancellation_finishes_adoption_then_retires_only_lease() {
        run_guardian_domain_real_birth(RealBirthFixtureMode::InFlightCancellation);
    }

    #[test]
    #[ignore = "requires actual sealed candidate identity; run explicitly in strict RCH/DSR"]
    fn guardian_domain_successor_image_recovery_and_reopen() {
        run_guardian_domain_real_birth(RealBirthFixtureMode::SuccessorImageRecovery);
    }

    fn run_guardian_domain_real_birth(mode: RealBirthFixtureMode) {
        use frankenterm_pty_guardian::provision_guardian_token;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;

        // Reader threads report rejected replay/delivery through `log`. Keep
        // those errors visible in this real-process regression's transcript;
        // a missing marker alone cannot identify which boundary rejected it.
        struct RecoveryTestLogger;
        impl log::Log for RecoveryTestLogger {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.level() <= log::Level::Warn
            }

            fn log(&self, record: &log::Record<'_>) {
                if self.enabled(record.metadata()) {
                    eprintln!("{} {}: {}", record.level(), record.target(), record.args());
                }
            }

            fn flush(&self) {}
        }
        static LOGGER: RecoveryTestLogger = RecoveryTestLogger;
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Warn);
        }

        let _global_state = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        struct ResetSpillFactory;
        impl Drop for ResetSpillFactory {
            fn drop(&mut self) {
                config::set_scrollback_spill_sink_factory(None);
                config::use_test_configuration();
            }
        }
        let _reset_spill_factory = ResetSpillFactory;
        config::use_test_configuration();
        assert!(config::configuration().scrollback_tiered_enabled);
        // An otherwise valid pristine model still needs the live storage
        // capability when default tiered retention is enabled. Preserve that
        // refusal rather than disabling tiering to make this fixture pass.
        let missing_storage_config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(config::TermConfig::new());
        let pristine = Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::clone(&missing_storage_config),
            "FrankenTerm",
            config::wezterm_version(),
            Box::new(io::sink()),
        );
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = pristine.capture_recovery_checkpoint(limits).unwrap();
        let inert =
            TerminalCheckpointV2::decode_canonical_json(checkpoint.canonical_payload(), limits)
                .unwrap()
                .restore_inert(missing_storage_config)
                .unwrap();
        let failure = match inert.into_live(Box::new(io::sink())) {
            Ok(_) => panic!("tiered activation accepted a missing storage capability"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.into_parts().0,
            InertTerminalError::ScrollbackActivation(
                wezterm_term::config::ScrollbackActivationError::MissingStorageCapability
            )
        ));

        struct StopOwnedServices {
            children: Vec<std::process::Child>,
            release: PathBuf,
            births: PathBuf,
            finished: PathBuf,
        }
        impl Drop for StopOwnedServices {
            fn drop(&mut self) {
                // Both fixture children also self-exit after a finite wait, so
                // an assertion cannot strand a broker reader on an immortal child.
                let _ = std::fs::write(&self.release, b"release");
                let settle = Instant::now() + Duration::from_secs(2);
                while Instant::now() < settle {
                    let births = std::fs::metadata(&self.births).map_or(0, |meta| meta.len());
                    let finished = std::fs::metadata(&self.finished).map_or(0, |meta| meta.len());
                    if finished >= births {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                for child in &mut self.children {
                    let _ = child.kill();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }

        frankenterm_pty_guardian::guardian_runtime_build_identity()
            .expect("real domain proof requires sealed candidate identity");
        let executable = std::env::var_os("FT_GUARDIAN_TEST_EXECUTABLE").expect(
            "set FT_GUARDIAN_TEST_EXECUTABLE to the same-source sealed guardian executable",
        );
        let directory = tempfile::Builder::new()
            .prefix("ft-domain-")
            .tempdir_in(std::fs::canonicalize("/tmp").unwrap())
            .unwrap()
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!(
            "GUARDIAN_DOMAIN_SERVICE_LOG_DIRECTORY={}",
            directory.display()
        );
        // Normal mux startup installs this real spill backend before building
        // TermConfig. Keep the same storage implementations, isolated beneath
        // this fixture's retained directory rather than the user's cache.
        let scrollback_directory = directory.join("scrollback");
        config::set_scrollback_spill_sink_factory(Some(Arc::new(move |context| {
            let sink = crate::LiveScrollbackSpillSink::new(scrollback_directory.clone(), &context)
                .expect("initialize real domain scrollback storage");
            let sink = crate::deferred_scrollback::DeferredScrollbackSpillSink::new(Arc::new(sink))
                .expect("initialize real domain deferred scrollback storage");
            Some(Arc::new(sink))
        })));
        let broker_dir = directory.join("broker");
        let spawn_catalog = broker_dir.join("spawn-catalog");
        let lease_catalog = broker_dir.join("lease-catalog");
        for path in [&broker_dir, &spawn_catalog, &lease_catalog] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let token = directory.join("token");
        let broker_token = broker_dir.join("token");
        provision_guardian_token(&token).unwrap();
        provision_guardian_token(&broker_token).unwrap();
        let socket = directory.join("guardian.sock");
        let broker_socket = broker_dir.join("broker.sock");
        let release = directory.join("release-children");
        let births = directory.join("births");
        let signaled = directory.join("signaled");
        let finished = directory.join("finished");
        let post_registration = directory.join("post-registration");
        let post_successor = directory.join("post-successor");
        let child_pid_path = directory.join("child-pid");
        let mut services = StopOwnedServices {
            children: Vec::new(),
            release: release.clone(),
            births: births.clone(),
            finished: finished.clone(),
        };
        for (name, command, args) in [
            (
                "broker",
                "broker-serve",
                vec![
                    ("--socket-path", &broker_socket),
                    ("--token-path", &broker_token),
                    ("--spawn-catalog-path", &spawn_catalog),
                    ("--lease-catalog-path", &lease_catalog),
                ],
            ),
            (
                "guardian",
                "serve",
                vec![
                    ("--socket-path", &socket),
                    ("--token-path", &token),
                    ("--broker-socket-path", &broker_socket),
                    ("--broker-token-path", &broker_token),
                ],
            ),
        ] {
            let mut process = std::process::Command::new(&executable);
            process.arg(command).args(["--poll-interval-ms", "2"]);
            for (flag, path) in args {
                process.arg(flag).arg(path);
            }
            let log = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(format!("{name}.log")))
                .unwrap();
            process
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log);
            services.children.push(process.spawn().unwrap());
        }
        {
            let ready_deadline = Instant::now() + Duration::from_secs(5);
            while !socket.exists() || !broker_socket.exists() {
                assert!(
                    Instant::now() < ready_deadline,
                    "owned services did not create sockets"
                );
                assert!(
                    services
                        .children
                        .iter_mut()
                        .all(|child| child.try_wait().unwrap().is_none())
                );
                thread::sleep(Duration::from_millis(5));
            }
            let executor = promise::spawn::SimpleExecutor::new();
            let mux = Arc::new(Mux::new(None));
            let domain =
                Arc::new(GuardianDomain::new(&mux, socket.clone(), token.clone()).unwrap());
            let registered: Arc<dyn Domain> = domain.clone();
            mux.add_domain(&registered).unwrap();
            mux.set_default_domain(&registered).unwrap();
            let command = || {
                let mut command = portable_pty::CommandBuilder::new("/bin/sh");
                // The release file is normal cleanup. This approximately
                // 120-second emergency fuse is separate from the unchanged
                // five-second phase assertions, not an accepted latency bound.
                command.args(["-c", "trap 'printf S >>\"$FT_SIGNALED\"; exit 0' HUP TERM; echo $$ >>\"$FT_PID\"; printf B >>\"$FT_BIRTHS\"; printf guardian-domain-marker; n=0; while test ! -e \"$FT_POST_REGISTRATION\" && test ! -e \"$FT_RELEASE\" && test $n -lt 2400; do sleep 0.05; n=$((n+1)); done; if test -e \"$FT_POST_REGISTRATION\"; then printf guardian-domain-post-registration-marker; fi; while test ! -e \"$FT_POST_SUCCESSOR\" && test ! -e \"$FT_RELEASE\" && test $n -lt 2400; do sleep 0.05; n=$((n+1)); done; if test -e \"$FT_POST_SUCCESSOR\"; then printf guardian-domain-post-successor-marker; fi; while test ! -e \"$FT_POST_SECOND_SUCCESSOR\" && test ! -e \"$FT_RELEASE\" && test $n -lt 2400; do sleep 0.05; n=$((n+1)); done; if test -e \"$FT_POST_SECOND_SUCCESSOR\"; then printf guardian-domain-second-successor-marker; fi; while test ! -e \"$FT_RELEASE\" && test $n -lt 2400; do sleep 0.05; n=$((n+1)); done; printf D >>\"$FT_FINISHED\""]);
                command.env("FT_PID", &child_pid_path);
                command.env("FT_BIRTHS", &births);
                command.env("FT_SIGNALED", &signaled);
                command.env("FT_RELEASE", &release);
                command.env("FT_FINISHED", &finished);
                command.env("FT_POST_REGISTRATION", &post_registration);
                command.env("FT_POST_SUCCESSOR", &post_successor);
                command.env(
                    "FT_POST_SECOND_SUCCESSOR",
                    directory.join("post-second-successor"),
                );
                command
            };
            let size = TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            };
            let foreign_mux = Arc::new(Mux::new(None));
            assert!(
                promise::spawn::block_on(domain.spawn_unpublished_pane(
                    &foreign_mux,
                    size,
                    Some(command()),
                    None,
                ))
                .is_err(),
                "domain accepted another mux's session authority"
            );
            assert!(!births.exists());
            let unpublished = promise::spawn::block_on(domain.spawn_unpublished_pane(
                &mux,
                size,
                Some(command()),
                None,
            ))
            .unwrap();
            assert!(
                mux.iter_panes().is_empty(),
                "Domain birth published before commit"
            );
            let pane = unpublished.publish(&mux).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                let (_, lines) = pane.get_lines(0..24);
                if lines
                    .iter()
                    .any(|line| line.as_str().contains("guardian-domain-marker"))
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "published pane did not render real child output"
                );
                thread::sleep(Duration::from_millis(2));
            }
            if mode == RealBirthFixtureMode::BirthAndCancellation {
                assert_real_birth_image_roundtrip(
                    &mux, &pane, &executor, &directory, &socket, &token, size,
                );
            }
            if mode == RealBirthFixtureMode::SuccessorImageRecovery {
                let durable_pane_id = pane.guardian_spawn_custody().unwrap().original.pane_id;
                // The successor fixture must release every predecessor-owned
                // connection, including the domain's shared census transport.
                drop(domain);
                drop(registered);
                let (successor_mux, successor_pane) = assert_real_birth_successor_image_roundtrip(
                    &mux, pane, &executor, &directory, &socket, &token, size,
                );
                let (successor_session, _) = successor_mux
                    .topology_snapshot_authority()
                    .expect("derive successor session incarnation for cleanup");
                let successor_mux_incarnation = Uuid::from_bytes(successor_session.as_bytes());
                std::fs::write(&release, b"release").unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut census: Option<GuardianClient> = None;
                loop {
                    assert!(
                        Instant::now() < deadline,
                        "successor child did not exit cleanly"
                    );
                    while executor.try_tick().unwrap() {}
                    if census.is_none() {
                        match GuardianClient::connect(&socket, &token, successor_mux_incarnation) {
                            Ok(client) => census = Some(client),
                            Err(GuardianClientError::Io(_)) => {
                                thread::sleep(Duration::from_millis(2));
                                continue;
                            }
                            Err(error) => panic!("census connect failed during cleanup: {error}"),
                        }
                    }
                    match census.as_mut().unwrap().census_snapshot() {
                        Ok(rows) => {
                            let matching = rows
                                .iter()
                                .filter(|row| row.pane_id == durable_pane_id)
                                .collect::<Vec<_>>();
                            assert!(
                                matching.len() <= 1,
                                "multiple census entries match durable pane id {durable_pane_id}"
                            );
                            if matching.len() == 1 {
                                assert_eq!(matching[0].pane_id, durable_pane_id);
                                if let Some(exit_status) = matching[0].exit_status {
                                    assert_eq!(
                                        exit_status, 0,
                                        "successor child exited with unexpected non-zero status: {exit_status}"
                                    );
                                    break;
                                }
                            }
                        }
                        Err(GuardianClientError::Io(_)) => {
                            census = None;
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("census snapshot failed during cleanup: {error}"),
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(std::fs::read(&finished).unwrap(), b"D");
                assert!(!signaled.exists());
                let cleanup_deadline = Instant::now() + Duration::from_secs(5);
                while !successor_pane.is_dead() {
                    assert!(
                        Instant::now() < cleanup_deadline,
                        "published successor pane did not observe child exit"
                    );
                    while executor.try_tick().unwrap() {}
                    thread::sleep(Duration::from_millis(2));
                }
                while executor.try_tick().unwrap() {}
                if let Some(registration) = successor_mux.capture_pane_registration(&successor_pane)
                {
                    assert!(registration.retire_and_prune_if_current());
                }
                for pane in mux.iter_panes() {
                    if let Some(registration) = mux.capture_pane_registration(&pane) {
                        assert!(registration.retire_and_prune_if_current());
                    }
                }
                loop {
                    while executor.try_tick().unwrap() {}
                    let cleanup_snapshot = successor_mux.pane_removal_cleanup_snapshot();
                    if cleanup_snapshot.active_fences == 0
                        && cleanup_snapshot.outstanding_leases == 0
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < cleanup_deadline,
                        "successor pane removal cleanup did not settle within deadline"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                loop {
                    while executor.try_tick().unwrap() {}
                    let cleanup_snapshot = mux.pane_removal_cleanup_snapshot();
                    if cleanup_snapshot.active_fences == 0
                        && cleanup_snapshot.outstanding_leases == 0
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < cleanup_deadline,
                        "predecessor pane removal cleanup did not settle within deadline"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                while executor.try_tick().unwrap() {}
                drop(successor_pane);
                drop(successor_mux);
                drop(census);
                drop(foreign_mux);
                drop(mux);
                while executor.try_tick().unwrap() {}
                drop(executor);
                println!("GUARDIAN_DOMAIN_SUCCESSOR_IMAGE_RECOVERY_SUCCESS");
                return;
            }
            let cancel_in_flight = mode == RealBirthFixtureMode::InFlightCancellation;
            if cancel_in_flight {
                let coordinator = Arc::clone(domain.state.lock().census.as_ref().unwrap());
                // The real lease plan must acquire this shared coordinator
                // before Claim. Holding it prevents the worker from returning
                // a completed guard even after the actual child is running.
                let deadline = Instant::now() + Duration::from_secs(5);
                let held_census = loop {
                    if let Some(held) = coordinator.state.try_lock() {
                        break held;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "census coordinator remained busy"
                    );
                    thread::sleep(Duration::from_millis(2));
                };
                let mut spawn =
                    Box::pin(domain.spawn_unpublished_pane(&mux, size, Some(command()), None));
                let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
                assert!(std::future::Future::poll(spawn.as_mut(), &mut context).is_pending());
                let deadline = Instant::now() + Duration::from_secs(5);
                while std::fs::read(&births).is_ok_and(|bytes| bytes == b"B") {
                    match std::future::Future::poll(spawn.as_mut(), &mut context) {
                        std::task::Poll::Ready(Err(error)) => {
                            panic!("second birth failed before cancellation barrier: {error:#}");
                        }
                        std::task::Poll::Ready(Ok(_)) => {
                            panic!("second birth passed the held census barrier");
                        }
                        std::task::Poll::Pending => {}
                    }
                    assert!(Instant::now() < deadline, "second real child did not start");
                    thread::sleep(Duration::from_millis(2));
                }
                assert_eq!(std::fs::read(&births).unwrap(), b"BB");
                assert!(domain.admission.load(Ordering::Acquire));
                assert!(domain.state.lock().publication.is_none());
                drop(spawn);
                assert!(
                    domain.admission.load(Ordering::Acquire),
                    "cancelling the future released admission before its worker settled"
                );
                assert!(domain.state.lock().unadopted_birth.is_some());
                // This guard also releases first during assertion unwinding,
                // before the fixture waits for or stops any owned processes.
                drop(held_census);
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let state = domain.state.lock();
                    let adopted = state.publication.is_some();
                    drop(state);
                    if adopted && !domain.admission.load(Ordering::Acquire) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "cancelled birth did not finish adoption"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
            } else {
                let unpublished = promise::spawn::block_on(domain.spawn_unpublished_pane(
                    &mux,
                    size,
                    Some(command()),
                    None,
                ))
                .unwrap();
                drop(unpublished);
            }
            assert_eq!(mux.iter_panes().len(), 1);
            let expected_guardian = domain
                .state
                .lock()
                .census
                .as_ref()
                .unwrap()
                .guardian_incarnation();
            let mut census: Option<GuardianClient> = None;
            let mut snapshot = |deadline: Instant| loop {
                assert!(Instant::now() < deadline, "census phase deadline expired");
                if census.is_none() {
                    match GuardianClient::connect(&socket, &token, domain.mux_incarnation) {
                        Ok(client) => {
                            assert_eq!(client.guardian_incarnation(), expected_guardian);
                            assert_eq!(client.mux_incarnation(), domain.mux_incarnation);
                            census = Some(client);
                        }
                        Err(GuardianClientError::Io(_)) => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("census reconnect refused: {error}"),
                    }
                }
                match census.as_mut().unwrap().census_snapshot() {
                    Ok(rows) => break rows,
                    Err(GuardianClientError::Io(_)) => {
                        // Drop the partial snapshot and reconnect to the same
                        // guardian. Never combine pages from separate attempts.
                        census = None;
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("census snapshot refused: {error}"),
                }
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let rows = loop {
                let rows = snapshot(deadline);
                if rows
                    .iter()
                    .any(|row| row.status == GuardianCensusPaneStatus::LiveUnclaimed)
                {
                    break rows;
                }
                assert!(
                    Instant::now() < deadline,
                    "cancelled pane lease was not retired"
                );
                thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows.iter()
                    .filter(|row| row.status == GuardianCensusPaneStatus::LiveUnclaimed)
                    .count(),
                1
            );
            assert_eq!(
                rows.iter()
                    .filter(|row| row.status == GuardianCensusPaneStatus::LiveClaimed)
                    .count(),
                1
            );
            assert_eq!(
                std::fs::read(&births).unwrap(),
                b"BB",
                "exact replay duplicated a birth"
            );
            assert!(
                !signaled.exists(),
                "unpublished cancellation signaled an owned child"
            );
            assert!(domain.state.lock().unadopted_birth.is_some());
            assert!(
                !domain
                    .state
                    .lock()
                    .publication
                    .as_ref()
                    .unwrap()
                    .was_published()
            );
            assert!(
                promise::spawn::block_on(domain.spawn_unpublished_pane(
                    &mux,
                    size,
                    Some(command()),
                    None,
                ))
                .is_err(),
                "cancelled birth must remain fenced, not create a replacement child"
            );
            assert_eq!(std::fs::read(&births).unwrap(), b"BB");
            std::fs::write(&release, b"release").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                while executor.try_tick().unwrap() {}
                let rows = snapshot(deadline);
                if rows.len() == 2 && rows.iter().all(|row| row.exit_status == Some(0)) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned fixture children did not exit and reap"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(std::fs::read(&finished).unwrap(), b"DD");
            assert!(!signaled.exists());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pane.is_dead() {
                assert!(
                    Instant::now() < deadline,
                    "published pane did not observe child exit"
                );
                while executor.try_tick().unwrap() {}
                thread::sleep(Duration::from_millis(2));
            }
            while executor.try_tick().unwrap() {}
            if let Some(registration) = mux.capture_pane_registration(&pane) {
                assert!(registration.retire_and_prune_if_current());
            }
            loop {
                while executor.try_tick().unwrap() {}
                let cleanup_snapshot = mux.pane_removal_cleanup_snapshot();
                if cleanup_snapshot.active_fences == 0 && cleanup_snapshot.outstanding_leases == 0 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "pane removal cleanup did not settle within deadline"
                );
                thread::sleep(Duration::from_millis(2));
            }
            while executor.try_tick().unwrap() {}
            drop(pane);
            drop(snapshot);
            drop(census);
            drop(domain);
            drop(registered);
            drop(foreign_mux);
            drop(mux);
            while executor.try_tick().unwrap() {}
            drop(executor);
            println!("GUARDIAN_DOMAIN_REAL_BIRTH_SUCCESS");
            if cancel_in_flight {
                println!("GUARDIAN_DOMAIN_IN_FLIGHT_CANCELLATION_SUCCESS");
            }
        }
    }

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
        Checkpoint {
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            intent: GuardianCheckpointIntent,
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

    struct FakeCheckpointStageState {
        calls: Vec<(Uuid, GuardianCheckpointStageKindV1)>,
        lose_reply_once: VecDeque<GuardianCheckpointStageKindV1>,
        scope: Option<GuardianCheckpointScopeV1>,
        upload_id: Option<Uuid>,
        descriptor: Option<GuardianCheckpointDescriptorV1>,
        chunk_bytes: Option<u32>,
        total_chunks: Option<u32>,
        next_index: u32,
        payload: Zeroizing<Vec<u8>>,
        completion_id: Option<Uuid>,
        acked: bool,
    }

    impl FakeCheckpointStageState {
        fn new(lose_reply_once: impl IntoIterator<Item = GuardianCheckpointStageKindV1>) -> Self {
            Self {
                calls: Vec::new(),
                lose_reply_once: lose_reply_once.into_iter().collect(),
                scope: None,
                upload_id: None,
                descriptor: None,
                chunk_bytes: None,
                total_chunks: None,
                next_index: 0,
                payload: Zeroizing::new(Vec::new()),
                completion_id: None,
                acked: false,
            }
        }

        fn assert_bound_request(
            &self,
            scope: GuardianCheckpointScopeV1,
            upload_id: Uuid,
            descriptor: GuardianCheckpointDescriptorV1,
            chunk_bytes: u32,
            total_chunks: u32,
        ) {
            assert_eq!(self.scope, Some(scope));
            assert_eq!(self.upload_id, Some(upload_id));
            assert_eq!(self.descriptor, Some(descriptor));
            assert_eq!(self.chunk_bytes, Some(chunk_bytes));
            assert_eq!(self.total_chunks, Some(total_chunks));
        }

        fn current_reply(&self, upload_id: Uuid) -> GuardianCheckpointStageReplyV1 {
            let Some(descriptor) = self.descriptor else {
                return GuardianCheckpointStageReplyV1::Absent { upload_id };
            };
            if self.acked {
                return GuardianCheckpointStageReplyV1::Acked {
                    upload_id,
                    completion_id: self
                        .completion_id
                        .expect("acked fake stage retains completion identity"),
                    checkpoint_id: descriptor.checkpoint_id(),
                    boundary_id: descriptor.boundary_id(),
                    total_bytes: descriptor.total_bytes(),
                };
            }
            if let Some(completion_id) = self.completion_id {
                return GuardianCheckpointStageReplyV1::Sealed {
                    upload_id,
                    completion_id,
                    checkpoint_id: descriptor.checkpoint_id(),
                    boundary_id: descriptor.boundary_id(),
                    total_bytes: descriptor.total_bytes(),
                };
            }
            let committed_bytes =
                u64::try_from(self.payload.len()).expect("fake Stage payload length fits u64");
            if self.next_index == 0 {
                GuardianCheckpointStageReplyV1::Ready {
                    upload_id,
                    next_index: 0,
                    committed_bytes,
                }
            } else {
                GuardianCheckpointStageReplyV1::Progress {
                    upload_id,
                    next_index: self.next_index,
                    committed_bytes,
                }
            }
        }
    }

    #[derive(Clone)]
    struct FakeCheckpointStageTransport {
        state: Arc<Mutex<FakeCheckpointStageState>>,
    }

    impl GuardianCheckpointStageTransport for FakeCheckpointStageTransport {
        fn checkpoint_stage(
            &mut self,
            request_id: Uuid,
            request: GuardianCheckpointStageRequestV1,
        ) -> Result<GuardianCheckpointStageReplyV1, GuardianProxyError> {
            let kind = request.kind();
            let scope = request.scope();
            let upload_id = request.upload_id();
            let descriptor = request.descriptor();
            let chunk_bytes = request.chunk_bytes();
            let total_chunks = request.total_chunks();
            let completion_id = request.completion_id();
            let mut state = self.state.lock();
            state.calls.push((request_id, kind));

            match kind {
                GuardianCheckpointStageKindV1::Begin => {
                    if state.scope.is_none() {
                        state.scope = Some(scope);
                        state.upload_id = Some(upload_id);
                        state.descriptor = Some(descriptor);
                        state.chunk_bytes = Some(chunk_bytes);
                        state.total_chunks = Some(total_chunks);
                    } else {
                        state.assert_bound_request(
                            scope,
                            upload_id,
                            descriptor,
                            chunk_bytes,
                            total_chunks,
                        );
                    }
                }
                GuardianCheckpointStageKindV1::Chunk => {
                    state.assert_bound_request(
                        scope,
                        upload_id,
                        descriptor,
                        chunk_bytes,
                        total_chunks,
                    );
                    let ((index, offset), bytes) = request
                        .into_chunk()
                        .and_then(|chunk| chunk.into_validated_parts())
                        .expect("fake Stage accepts a protocol-validated chunk");
                    assert_eq!(index, state.next_index);
                    assert_eq!(
                        offset,
                        u64::try_from(state.payload.len())
                            .expect("fake Stage payload offset fits u64")
                    );
                    state.payload.extend_from_slice(bytes.as_slice());
                    state.next_index = state
                        .next_index
                        .checked_add(1)
                        .expect("bounded fake Stage chunk index advances");
                }
                GuardianCheckpointStageKindV1::Seal => {
                    state.assert_bound_request(
                        scope,
                        upload_id,
                        descriptor,
                        chunk_bytes,
                        total_chunks,
                    );
                    assert_eq!(state.next_index, total_chunks);
                    assert_eq!(
                        u64::try_from(state.payload.len())
                            .expect("fake Stage payload length fits u64"),
                        descriptor.total_bytes()
                    );
                    request
                        .validate_staged_plaintext(state.payload.as_slice())
                        .expect("fake Stage validates the production terminal commitment");
                    state.completion_id = Some(id(0x8f0));
                }
                GuardianCheckpointStageKindV1::Query => {
                    if state.scope.is_some() {
                        state.assert_bound_request(
                            scope,
                            upload_id,
                            descriptor,
                            chunk_bytes,
                            total_chunks,
                        );
                    }
                }
                GuardianCheckpointStageKindV1::Ack => {
                    state.assert_bound_request(
                        scope,
                        upload_id,
                        descriptor,
                        chunk_bytes,
                        total_chunks,
                    );
                    assert_eq!(completion_id, state.completion_id);
                    state.acked = true;
                }
            }

            let reply = state.current_reply(upload_id);
            let lose_reply = state
                .lose_reply_once
                .iter()
                .position(|candidate| *candidate == kind)
                .and_then(|position| state.lose_reply_once.remove(position))
                .is_some();
            if lose_reply {
                Err(GuardianProxyError::Client(GuardianClientError::Io(
                    io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "injected checkpoint Stage lost reply",
                    ),
                )))
            } else {
                Ok(reply)
            }
        }
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
        delivered: SyncSender<()>,
    }

    impl GuardianLiveOutputReader for ChannelGuardianReader {
        fn deliver_next_record(
            &mut self,
            deliver: &mut dyn FnMut(
                GuardianOutputSegmentIdentity,
                GuardianOutputAppendReceipt,
                Arc<[u8]>,
            ) -> io::Result<()>,
        ) -> io::Result<GuardianLiveOutputDelivery> {
            let (segment, output, payload) = self
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "test guardian reader closed")
                    }
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        io::Error::new(io::ErrorKind::TimedOut, "test guardian reader timed out")
                    }
                })?;
            deliver(segment, output, payload)?;
            self.delivered.send(()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test guardian delivery acknowledgement receiver closed",
                )
            })?;
            Ok(GuardianLiveOutputDelivery::replay_page_acknowledged())
        }
    }

    fn capture_real_guardian_checkpoint(
        mux: &Arc<Mux>,
        pane_id: mux::pane::PaneId,
        executor: &promise::spawn::SimpleExecutor,
    ) -> mux::guardian_checkpoint::PublishedGuardianCheckpoint {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let capture_mux = Arc::clone(mux);
        let capture_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let operation = capture_mux.capture_pane_operation(pane_id).unwrap();
                let result = operation.capture_current_guardian_checkpoint(
                    TerminalCheckpointLimits::default(),
                    Duration::from_secs(5),
                );
                if matches!(
                    result
                        .as_ref()
                        .err()
                        .and_then(|error| error.downcast_ref::<mux::LiveParserCheckpointError>()),
                    Some(
                        mux::LiveParserCheckpointError::CheckpointBusy
                            | mux::LiveParserCheckpointError::GuardianDeliveryBusy
                    )
                ) && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                }
                tx.send(result).unwrap();
                break;
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let published = loop {
            while executor.try_tick().unwrap() {}
            if let Ok(result) = rx.try_recv() {
                break result.unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "real guardian capture did not settle"
            );
            thread::sleep(Duration::from_millis(2));
        };
        capture_thread.join().unwrap();
        published
    }

    fn assert_real_birth_image_roundtrip(
        mux: &Arc<Mux>,
        pane: &Arc<dyn mux::pane::Pane>,
        executor: &promise::spawn::SimpleExecutor,
        directory: &Path,
        socket: &Path,
        token: &Path,
        size: TerminalSize,
    ) {
        use frankenterm_core::mux_recovery_image::{
            MuxRecoveryImage, RecoveryImageGenerationMeta, RecoveryObjectRef,
            RecoveryParserCheckpoint, RecoverySpawnCustody,
        };
        use frankenterm_core::session_restore::{
            WholeMuxRecoveryVerifier, WholeMuxTrustedIdentityConfig,
            reconstruct_whole_mux_image_inert, semantic_object_id_from_str,
        };
        use frankenterm_core::snapshot_publication::{
            GenerationRootPublishRequest, RecoveryObjectPayload, SnapshotPublicationStore,
        };
        use frankenterm_core::snapshot_representation::{
            ObjectMetadata, RecoveryKey, RecoveryObjectKind, encode_recovery_object,
        };
        let provenance = pane
            .guardian_spawn_custody()
            .expect("real birth retained authenticated provenance");
        assert_eq!(
            provenance.original.pane_id.as_bytes(),
            &pane.durable_pane_id().unwrap()
        );
        assert_eq!(provenance.current_lease_generation, 1);
        assert_eq!(
            provenance.current_mux_incarnation,
            provenance.original.mux_incarnation
        );
        let original_registration = mux.capture_pane_registration(pane).unwrap();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        tab.assign_pane(pane);
        // The domain has already registered this live pane. Bind its populated
        // tab through the structural-owner path without starting another reader.
        let registration = mux.add_tab_and_active_pane(&tab).unwrap().unwrap();
        assert!(registration.same_registration(&original_registration));
        let window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, *window).unwrap();
        drop(window);

        let quiet = capture_real_guardian_checkpoint(mux, pane.pane_id(), executor);
        let quiet_sequence = quiet.capture().output_sequence();
        let quiet_bytes = quiet.capture().journal_cumulative_plaintext_bytes();
        let quiet_receipt = quiet.receipt();
        drop(quiet);
        let post_registration = directory.join("post-registration");
        std::fs::write(&post_registration, b"step").unwrap();
        let post_registration_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            let (_, lines) = pane.get_lines(0..24);
            if lines.iter().any(|line| {
                line.as_str()
                    .contains("guardian-domain-post-registration-marker")
            }) {
                break;
            }
            assert!(
                Instant::now() < post_registration_deadline,
                "published pane did not render fresh child output after registration: {:?}",
                lines.iter().map(|line| line.as_str()).collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(2));
        }

        let pane_id = pane.pane_id();
        let published = capture_real_guardian_checkpoint(mux, pane_id, executor);
        assert!(published.capture().output_sequence() > quiet_sequence);
        assert!(published.capture().journal_cumulative_plaintext_bytes() > quiet_bytes);
        assert_ne!(published.receipt(), quiet_receipt);
        let repeated = capture_real_guardian_checkpoint(mux, pane_id, executor);
        assert_eq!(
            repeated.receipt(),
            published.receipt(),
            "an unchanged live capture must reuse the fully acknowledged publication"
        );
        assert!(mux.guardian_checkpoint_is_current(pane_id, &repeated));
        drop(repeated);
        let captured = mux.capture_topology_coherent(Default::default()).unwrap();
        assert_eq!(captured.pane_bindings.len(), 1);
        assert_eq!(captured.pane_bindings[0].spawn_custody, Some(provenance));
        let key = Arc::new(RecoveryKey::from_bytes([0x51; 32]).unwrap());
        let object_id = "real-guardian-terminal";
        let timestamp = captured.captured_at_epoch_ms;
        let encode = |payload: &[u8], id, kind| {
            encode_recovery_object(
                payload,
                ObjectMetadata::single(id, kind, 1, None, timestamp),
                &key,
                None,
            )
            .unwrap()
            .to_bytes()
            .unwrap()
        };
        let ciphertext = encode(
            published
                .capture()
                .terminal_checkpoint()
                .canonical_payload(),
            semantic_object_id_from_str(object_id),
            RecoveryObjectKind::TerminalCheckpoint,
        );
        let digest: [u8; 32] = Sha256::digest(&ciphertext).into();
        let references = HashMap::from([(
            pane_id,
            RecoveryObjectRef {
                object_id: object_id.into(),
                byte_length: ciphertext.len() as u64,
                payload_digest: digest,
                schema_version: 2,
            },
        )]);
        let parser_checkpoints =
            HashMap::from([(pane_id, RecoveryParserCheckpoint::Guardian(&published))]);
        let image = MuxRecoveryImage::from_mux_captured_checkpoints(
            RecoveryImageGenerationMeta {
                generation: 1,
                predecessor_digest: None,
                created_at_epoch_ms: timestamp,
                ft_version: config::wezterm_version().to_owned(),
                session_id: "real-guardian-birth".into(),
            },
            &captured,
            &parser_checkpoints,
            &references,
        )
        .unwrap();
        let image_bytes = image.to_canonical_json().unwrap();
        assert_eq!(
            MuxRecoveryImage::from_json_slice(&image_bytes).unwrap(),
            image
        );
        for control in 0..4 {
            let mut changed = image.clone();
            if control == 0 {
                changed.panes[0].checkpoint.authority =
                    frankenterm_core::mux_recovery_image::CheckpointAuthority::ModelOnly {
                        captured_at_epoch_ms: timestamp,
                        parser_seqno: None,
                    };
            } else if let RecoverySpawnCustody::Original {
                current_lease_generation,
                current_mux_incarnation,
                guardian_incarnation,
                ..
            } = &mut changed.panes[0].spawn_custody
            {
                match control {
                    1 => *current_lease_generation += 1,
                    2 => *current_mux_incarnation = Uuid::new_v4(),
                    _ => *guardian_incarnation = Uuid::new_v4(),
                }
            }
            changed.image_digest = changed.compute_digest().unwrap();
            assert!(
                MuxRecoveryImage::from_json_slice(&serde_json::to_vec(&changed).unwrap()).is_err()
            );
        }
        let store =
            SnapshotPublicationStore::open(directory.join("whole-image"), Default::default())
                .unwrap();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: object_id.into(),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: ciphertext,
            })
            .unwrap();
        let root = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "actual-birth".into(),
            predecessor: None,
            manifest_bytes: encode(&image_bytes, [0x72; 32], RecoveryObjectKind::WholeMuxImage),
            created_at_ms: timestamp,
        };
        let mut verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::clone(&key),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        );
        assert!(
            store.publish_generation_root(&root, &verifier).is_err(),
            "serialized Guardian identity alone is not authenticated authority"
        );
        verifier
            .register_published_guardian_capture(&published)
            .unwrap();
        store.publish_generation_root(&root, &verifier).unwrap();
        let selector = frankenterm_pty_guardian::GuardianCheckpointAdoptionSelectorV1 {
            checkpoint_id: published
                .receipt()
                .intent()
                .checkpoint_identity()
                .into_bytes(),
            capturing_mux_incarnation: provenance.current_mux_incarnation,
            capture_generation: published.receipt().generation(),
            adoption_effect_id: published.receipt().effect_id(),
            adoption_sequence: published.receipt().sequence(),
        };
        let reopened = GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
            .unwrap()
            .reopen_checkpoint(selector)
            .unwrap();
        assert_eq!(
            reopened.payload_digest(),
            published.capture().terminal_payload_digest(),
            "live and durable witnesses must use the same guardian digest domain"
        );
        assert_ne!(
            reopened.payload_digest(),
            <[u8; 32]>::from(Sha256::digest(
                published
                    .capture()
                    .terminal_checkpoint()
                    .canonical_payload()
            )),
            "plain SHA-256 must not stand in for guardian checkpoint identity"
        );
        let mut wrong_scope = provenance.original;
        wrong_scope.effect_id = Uuid::new_v4();
        assert!(GuardianDurableSpawnCustodyV1::open_existing(token, wrong_scope).is_err());

        let mut wrong_checkpoint = selector;
        wrong_checkpoint.checkpoint_id = [0x19; 32];
        assert!(
            GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
                .unwrap()
                .reopen_checkpoint(wrong_checkpoint)
                .is_err()
        );

        let mut wrong_mux = selector;
        wrong_mux.capturing_mux_incarnation = Uuid::new_v4();
        assert!(
            GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
                .unwrap()
                .reopen_checkpoint(wrong_mux)
                .is_err()
        );

        let mut wrong_gen = selector;
        wrong_gen.capture_generation = 99;
        assert!(
            GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
                .unwrap()
                .reopen_checkpoint(wrong_gen)
                .is_err()
        );

        let mut wrong_effect = selector;
        wrong_effect.adoption_effect_id = Uuid::new_v4();
        assert!(
            GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
                .unwrap()
                .reopen_checkpoint(wrong_effect)
                .is_err()
        );

        let mut wrong_seq = selector;
        wrong_seq.adoption_sequence = 99;
        assert!(
            GuardianDurableSpawnCustodyV1::open_existing(token, provenance.original)
                .unwrap()
                .reopen_checkpoint(wrong_seq)
                .is_err()
        );
        // A new test process has no in-memory publication witness, capture,
        // or opened store. It must recover authority from existing bytes.
        let child_log_path = directory.join("fresh-image-verifier.stdout");
        let child_log = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&child_log_path)
            .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guardian_proxy::tests::guardian_image_fresh_process_existing_custody",
                "--nocapture",
            ])
            .env("FT_TEST_IMAGE_REOPEN_ROOT", directory.join("whole-image"))
            .env("FT_TEST_IMAGE_REOPEN_TOKEN", token)
            .env_remove("FT_TEST_IMAGE_REOPEN_SCOPE")
            .env_remove("FT_TEST_IMAGE_REOPEN_CHECKPOINT")
            .env_remove("FT_TEST_IMAGE_EXPECTED_GENERATION")
            .env_remove("FT_TEST_IMAGE_EXPECTED_MARKER")
            .stdout(child_log)
            .spawn()
            .unwrap();
        let child_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fresh process could not authenticate durable image"
                );
                break;
            }
            if Instant::now() >= child_deadline {
                child
                    .kill()
                    .expect("settle only owned image verifier child");
                child.wait().unwrap();
                panic!("fresh process image verifier deadline expired");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let child_log = std::fs::read_to_string(child_log_path).unwrap();
        assert!(
            child_log.contains("running 1 test") && child_log.contains("1 passed; 0 failed"),
            "fresh verifier did not execute exactly one test: {}",
            child_log
        );
        let validated = store
            .select_verified_roots(&verifier)
            .unwrap()
            .current
            .unwrap();
        let restored = reconstruct_whole_mux_image_inert(
            &validated,
            TerminalCheckpointLimits::default(),
            None,
            &std::collections::HashSet::<String>::new(),
        )
        .unwrap();
        let restored_pane = &restored.pane_terminals[&(pane_id as u64)];
        assert_eq!(restored_pane.spawn_custody, image.panes[0].spawn_custody);
        assert_eq!(
            restored_pane
                .terminal
                .checkpoint()
                .unwrap()
                .to_canonical_json(TerminalCheckpointLimits::default())
                .unwrap()
                .as_slice(),
            published
                .capture()
                .terminal_checkpoint()
                .canonical_payload()
        );
        let checkpoint_json: serde_json::Value = serde_json::from_slice(
            published
                .capture()
                .terminal_checkpoint()
                .canonical_payload(),
        )
        .unwrap();
        let restored_text: String = checkpoint_json["primary_screen"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|line| line["cells"].as_array().unwrap())
            .map(|cell| cell["text"].as_str().unwrap())
            .collect();
        assert!(
            restored_text.contains("guardian-domain-post-registration-marker"),
            "guardian terminal checkpoint did not contain post-registration child output"
        );
        let successor_census = Arc::new(
            GuardianCensusCoordinator::connect(
                socket,
                token,
                provenance.original.guardian_incarnation,
                Uuid::new_v4(),
            )
            .unwrap(),
        );
        let plan = GuardianProxyLeasePlan::prepare_from_recovery(
            socket,
            token,
            PtySize {
                rows: size.rows as u16,
                cols: size.cols as u16,
                pixel_width: size.pixel_width as u16,
                pixel_height: size.pixel_height as u16,
            },
            successor_census,
            &validated,
            provenance.original.pane_id,
        )
        .unwrap();
        assert!(
            plan.claim(
                provenance.original.pane_id,
                1,
                Uuid::new_v4(),
                Uuid::new_v4()
            )
            .is_err(),
            "live predecessor still fences restored successor plan"
        );
    }

    fn assert_real_birth_successor_image_roundtrip(
        mux: &Arc<Mux>,
        pane: Arc<dyn mux::pane::Pane>,
        executor: &promise::spawn::SimpleExecutor,
        directory: &Path,
        socket: &Path,
        token: &Path,
        size: TerminalSize,
    ) -> (Arc<Mux>, Arc<dyn mux::pane::Pane>) {
        use frankenterm_core::mux_recovery_image::{
            MuxRecoveryImage, RecoveryImageGenerationMeta, RecoveryObjectRef,
            RecoveryParserCheckpoint,
        };
        use frankenterm_core::session_restore::{
            WholeMuxRecoveryVerifier, WholeMuxTrustedIdentityConfig,
            reconstruct_whole_mux_image_inert, semantic_object_id_from_str,
        };
        use frankenterm_core::snapshot_engine::{
            WholeMuxPublicationIdentity, capture_and_publish_whole_mux_recovery,
        };
        use frankenterm_core::snapshot_publication::{
            GenerationRootPublishRequest, PredecessorBinding, PublicationError,
            RecoveryObjectPayload, SnapshotPublicationStore,
        };
        use frankenterm_core::snapshot_representation::{
            ObjectMetadata, RecoveryKey, RecoveryObjectKind, encode_recovery_object,
        };

        let provenance = pane
            .guardian_spawn_custody()
            .expect("real birth retained authenticated provenance");
        assert_eq!(
            provenance.original.pane_id.as_bytes(),
            &pane.durable_pane_id().unwrap()
        );
        assert_eq!(provenance.current_lease_generation, 1);
        assert_eq!(
            provenance.current_mux_incarnation,
            provenance.original.mux_incarnation
        );
        let original_registration = mux.capture_pane_registration(&pane).unwrap();
        let tab = Arc::new(mux::tab::Tab::new(&size));
        tab.assign_pane(&pane);
        let registration = mux.add_tab_and_active_pane(&tab).unwrap().unwrap();
        assert!(registration.same_registration(&original_registration));
        let window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, *window).unwrap();
        drop(window);

        let quiet = capture_real_guardian_checkpoint(mux, pane.pane_id(), executor);
        let quiet_sequence = quiet.capture().output_sequence();
        let quiet_bytes = quiet.capture().journal_cumulative_plaintext_bytes();
        let post_registration = directory.join("post-registration");
        std::fs::write(&post_registration, b"step").unwrap();
        let post_registration_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            let (_, lines) = pane.get_lines(0..24);
            if lines.iter().any(|line| {
                line.as_str()
                    .contains("guardian-domain-post-registration-marker")
            }) {
                break;
            }
            assert!(
                Instant::now() < post_registration_deadline,
                "published pane did not render fresh child output after registration"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let pane_id = pane.pane_id();
        let published = capture_real_guardian_checkpoint(mux, pane_id, executor);
        assert!(published.capture().output_sequence() > quiet_sequence);
        assert!(published.capture().journal_cumulative_plaintext_bytes() > quiet_bytes);

        // Predecessor live stage: common-cut validation before detach
        assert!(
            mux.guardian_checkpoint_is_current(pane_id, &published),
            "fresh live guardian capture must be current before detach"
        );
        assert!(
            !mux.guardian_checkpoint_is_current(pane_id, &quiet),
            "quiet capture before child output must be stale (detected ABA/mutation)"
        );
        drop(quiet);
        let captured = mux.capture_topology_coherent(Default::default()).unwrap();
        assert_eq!(captured.pane_bindings.len(), 1);
        assert_eq!(captured.pane_bindings[0].spawn_custody, Some(provenance));
        let key = Arc::new(RecoveryKey::from_bytes([0x51; 32]).unwrap());
        let object_id = "real-guardian-terminal";
        let timestamp = captured.captured_at_epoch_ms;
        let encode = |payload: &[u8], id, kind| {
            encode_recovery_object(
                payload,
                ObjectMetadata::single(id, kind, 1, None, timestamp),
                &key,
                None,
            )
            .unwrap()
            .to_bytes()
            .unwrap()
        };
        let ciphertext = encode(
            published
                .capture()
                .terminal_checkpoint()
                .canonical_payload(),
            semantic_object_id_from_str(object_id),
            RecoveryObjectKind::TerminalCheckpoint,
        );
        let digest: [u8; 32] = Sha256::digest(&ciphertext).into();
        let references = HashMap::from([(
            pane_id,
            RecoveryObjectRef {
                object_id: object_id.into(),
                byte_length: ciphertext.len() as u64,
                payload_digest: digest,
                schema_version: 2,
            },
        )]);
        let parser_checkpoints =
            HashMap::from([(pane_id, RecoveryParserCheckpoint::Guardian(&published))]);
        let image = MuxRecoveryImage::from_mux_captured_checkpoints(
            RecoveryImageGenerationMeta {
                generation: 1,
                predecessor_digest: None,
                created_at_epoch_ms: timestamp,
                ft_version: config::wezterm_version().to_owned(),
                session_id: "real-guardian-birth".into(),
            },
            &captured,
            &parser_checkpoints,
            &references,
        )
        .unwrap();
        let image_bytes = image.to_canonical_json().unwrap();

        let store =
            SnapshotPublicationStore::open(directory.join("whole-image"), Default::default())
                .unwrap();
        store
            .publish_object(&RecoveryObjectPayload {
                object_id: object_id.into(),
                expected_sha256: hex::encode(digest),
                ciphertext_bytes: ciphertext,
            })
            .unwrap();
        let root = GenerationRootPublishRequest {
            generation: 1,
            publisher_id: "actual-birth".into(),
            predecessor: None,
            manifest_bytes: encode(&image_bytes, [0x72; 32], RecoveryObjectKind::WholeMuxImage),
            created_at_ms: timestamp,
        };
        let mut verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::clone(&key),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        );
        verifier
            .register_published_guardian_capture(&published)
            .unwrap();
        let root_receipt = store.publish_generation_root(&root, &verifier).unwrap();

        let recovery_store = SnapshotPublicationStore::open(
            directory.join("whole-image-common-cut"),
            Default::default(),
        )
        .unwrap();
        let recovery_expected_gen1 = WholeMuxPublicationIdentity {
            generation: 1,
            session_id: "real-guardian-birth".into(),
            mux_incarnation_id: hex::encode(captured.session_incarnation.as_bytes()),
            root_object_id: [0x72; 32],
            publisher_id: "actual-birth".into(),
            ft_version: config::wezterm_version().to_owned(),
            predecessor: None,
            predecessor_image_digest: None,
            existing_guardian_custody: None,
        };
        let recovery_gen1_receipt = {
            let thread_cx = frankenterm_core::cx::Cx::for_testing();
            let thread_mux = Arc::clone(mux);
            let thread_store = SnapshotPublicationStore::open(
                directory.join("whole-image-common-cut"),
                Default::default(),
            )
            .unwrap();
            let thread_key = Arc::clone(&key);
            let thread_expected = recovery_expected_gen1;
            let handle = thread::spawn(move || {
                capture_and_publish_whole_mux_recovery(
                    &thread_cx,
                    &thread_mux,
                    &thread_store,
                    thread_key,
                    &thread_expected,
                    Duration::from_secs(5),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                while executor.try_tick().unwrap() {}
                if handle.is_finished() {
                    break handle
                        .join()
                        .unwrap()
                        .expect("seed generation 1 via whole-mux recovery capture before detach");
                }
                assert!(Instant::now() < deadline, "gen1 recovery capture timed out");
                thread::sleep(Duration::from_millis(2));
            }
        };
        assert_eq!(recovery_gen1_receipt.generation, 1);
        let recovery_gen1_verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::clone(&key),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        )
        .with_existing_guardian_custody(token.to_path_buf());
        let recovery_gen1_selected = recovery_store
            .select_verified_roots(&recovery_gen1_verifier)
            .expect("verifier must select verified generation 1 root from common-cut store");
        let recovery_gen1_validated = recovery_gen1_selected
            .current
            .expect("must verify published generation 1 root");
        let recovery_gen1_image_digest = recovery_gen1_validated.image().image_digest;
        let recovery_gen1_hash = recovery_gen1_receipt.sha256;

        let child_log_path = directory.join("fresh-image-verifier.stdout");
        let child_log = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&child_log_path)
            .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guardian_proxy::tests::guardian_image_fresh_process_existing_custody",
                "--nocapture",
            ])
            .env("FT_TEST_IMAGE_REOPEN_ROOT", directory.join("whole-image"))
            .env("FT_TEST_IMAGE_REOPEN_TOKEN", token)
            .env_remove("FT_TEST_IMAGE_REOPEN_SCOPE")
            .env_remove("FT_TEST_IMAGE_REOPEN_CHECKPOINT")
            .env_remove("FT_TEST_IMAGE_EXPECTED_GENERATION")
            .env_remove("FT_TEST_IMAGE_EXPECTED_MARKER")
            .stdout(child_log)
            .spawn()
            .unwrap();
        let child_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fresh process could not authenticate durable image"
                );
                break;
            }
            if Instant::now() >= child_deadline {
                child
                    .kill()
                    .expect("settle only owned image verifier child");
                child.wait().unwrap();
                panic!("fresh process image verifier deadline expired");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let child_log = std::fs::read_to_string(child_log_path).unwrap();
        assert!(
            child_log.contains("running 1 test") && child_log.contains("1 passed; 0 failed"),
            "fresh gen1 verifier did not execute exactly one test: {}",
            child_log
        );

        let validated = store
            .select_verified_roots(&verifier)
            .unwrap()
            .current
            .unwrap();
        let restored = reconstruct_whole_mux_image_inert(
            &validated,
            TerminalCheckpointLimits::default(),
            None,
            &std::collections::HashSet::<String>::new(),
        )
        .unwrap();
        let restored_pane = &restored.pane_terminals[&(pane_id as u64)];
        assert_eq!(restored_pane.rows as usize, size.rows);
        assert_eq!(restored_pane.cols as usize, size.cols);

        let child_pid_path = directory.join("child-pid");
        let post_successor = directory.join("post-successor");
        let births = directory.join("births");
        let signaled = directory.join("signaled");
        let child_pid_text = std::fs::read_to_string(&child_pid_path).unwrap();
        let original_pid: i32 = child_pid_text
            .lines()
            .next()
            .expect("child recorded pid")
            .trim()
            .parse()
            .expect("valid child pid");
        assert!(original_pid > 0);

        let is_child_alive = |pid: i32| -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|status| status.success())
        };
        assert!(is_child_alive(original_pid));

        // 1. Detach predecessor registration and verify stale predecessor cannot act
        assert!(registration.detach_local_if_current());
        assert!(mux.capture_pane_registration(&pane).is_none());
        assert!(registration.try_with_current(|_| ()).is_none());
        assert!(registration.try_with_current_output(|_| ()).is_none());
        assert!(registration.operation_guard(mux).is_none());
        drop(registration);
        drop(original_registration);

        // 2. Remove tab via local-only (non-signaling) lifecycle and release predecessor Arc references
        let predecessor_domain_id = pane.domain_id();
        assert!(mux.remove_tab_local_only_if_same(&tab));
        let predecessor_domain = mux.get_domain(predecessor_domain_id).unwrap();
        assert!(mux.domain_was_detached_if_guard(&predecessor_domain));
        drop(predecessor_domain);
        drop(tab);
        drop(pane);

        // 3. Tick/drain actual retained operations through the executor
        while executor.try_tick().unwrap() {}

        // Verify child process survived predecessor drop without signals
        assert_eq!(std::fs::read(&births).unwrap(), b"B");
        assert!(!signaled.exists());
        assert!(is_child_alive(original_pid));

        let successor_mux = Arc::new(Mux::new(None));
        let (successor_session, _) = successor_mux
            .topology_snapshot_authority()
            .expect("derive successor session incarnation from topology authority");
        let successor_mux_incarnation = Uuid::from_bytes(successor_session.as_bytes());

        // Observe through the successor identity. Reconnecting as the old mux
        // would re-enroll its transport ownership and forbid successor rotation.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut successor_observer: Option<GuardianClient> = None;
        let post_retire_entry = loop {
            assert!(
                Instant::now() < deadline,
                "predecessor lease was not retired to LiveUnclaimed in census"
            );
            while executor.try_tick().unwrap() {}
            if successor_observer.is_none() {
                match GuardianClient::connect(socket, token, successor_mux_incarnation) {
                    Ok(client) => successor_observer = Some(client),
                    Err(GuardianClientError::Io(_)) => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("census connect failed: {error}"),
                }
            }
            match successor_observer.as_mut().unwrap().census_snapshot() {
                Ok(entries) => {
                    if let Some(entry) = entries
                        .iter()
                        .find(|entry| entry.pane_id == provenance.original.pane_id)
                    {
                        if entry.status
                            == mux::guardian_protocol::GuardianCensusPaneStatus::LiveUnclaimed
                        {
                            break entry.clone();
                        }
                    }
                }
                Err(GuardianClientError::Io(_)) => {
                    successor_observer = None;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("census snapshot failed: {error}"),
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(post_retire_entry.generation, 1);

        // 5. Successor coordinator connection and claim generation 2 on retired lease under successor Mux
        let successor_domain = Arc::new(
            GuardianDomain::new(&successor_mux, socket.to_path_buf(), token.to_path_buf()).unwrap(),
        );
        assert_eq!(
            successor_mux_incarnation, successor_domain.mux_incarnation,
            "derived successor mux incarnation must match guardian domain owner identity"
        );
        let registered_successor_domain: Arc<dyn Domain> = successor_domain.clone();
        successor_mux
            .add_domain(&registered_successor_domain)
            .unwrap();
        successor_mux
            .set_default_domain(&registered_successor_domain)
            .unwrap();

        let successor_coordinator = Arc::new(
            GuardianCensusCoordinator::connect(
                socket,
                token,
                provenance.original.guardian_incarnation,
                successor_mux_incarnation,
            )
            .unwrap(),
        );

        // Pane retirement can precede the guardian readiness loop observing
        // the last predecessor transport EOF. A definite rejection admits no
        // effect; wait for full owner retirement using the same request IDs.
        // Never retry an ambiguous failure or manufacture a successful lease.
        let claim_request_id = Uuid::new_v4();
        let claim_effect_id = Uuid::new_v4();
        let claim_deadline = Instant::now() + Duration::from_secs(5);
        let staging = loop {
            while executor.try_tick().unwrap() {}
            let successor_plan = GuardianProxyLeasePlan::prepare_from_recovery(
                socket,
                token,
                PtySize {
                    rows: size.rows as u16,
                    cols: size.cols as u16,
                    pixel_width: size.pixel_width as u16,
                    pixel_height: size.pixel_height as u16,
                },
                Arc::clone(&successor_coordinator),
                &validated,
                provenance.original.pane_id,
            )
            .unwrap();
            match successor_plan.claim(
                provenance.original.pane_id,
                1,
                claim_request_id,
                claim_effect_id,
            ) {
                Ok(staging) => break staging,
                Err(GuardianProxyError::Client(GuardianClientError::Rejected(
                    GuardianRejectionCode::InvalidRequest,
                ))) if Instant::now() < claim_deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("successor claim after owner retirement failed: {error}"),
            }
        };

        let successor_pane_id = alloc_pane_id().expect("allocate successor pane id");
        assert_ne!(
            successor_pane_id, pane_id,
            "successor local pane id must be distinct from predecessor local pane id"
        );
        let successor_domain_id = registered_successor_domain.domain_id();
        assert_ne!(
            successor_domain_id, predecessor_domain_id,
            "successor domain id must be freshly allocated and distinct from predecessor domain"
        );
        let successor_description =
            format!("recovered guardian pane {}", provenance.original.pane_id);
        // Match real pane birth: activation requires a storage capability
        // bound to this successor pane and its preserved durable identity.
        // TermConfig::new intentionally has no pane-specific spill backend.
        let successor_term_config: Arc<dyn TerminalConfiguration> =
            Arc::new(config::TermConfig::new_for_pane(
                successor_pane_id,
                successor_domain_id,
                *provenance.original.pane_id.as_bytes(),
                successor_description.clone(),
            ));
        let activated = staging
            .restore_and_activate(successor_term_config, TerminalCheckpointLimits::default())
            .expect("successor restore_and_activate must succeed");
        let local_pane = activated.into_local_pane(
            successor_pane_id,
            successor_domain_id,
            successor_description,
        );
        let unpublished = mux::domain::UnpublishedPane::from_guardian_proxy(local_pane)
            .expect("local pane converts to unpublished pane");
        let successor_pane = unpublished
            .publish(&successor_mux)
            .expect("successor pane publishes to successor mux");
        assert_eq!(successor_pane.pane_id(), successor_pane_id);
        assert_ne!(
            successor_pane.pane_id(),
            pane_id,
            "successor local pane id must be distinct from predecessor local pane id"
        );
        assert_eq!(successor_pane.domain_id(), successor_domain_id);

        let successor_registration = successor_mux
            .capture_pane_registration(&successor_pane)
            .unwrap();
        let successor_tab = Arc::new(mux::tab::Tab::new(&size));
        successor_tab.assign_pane(&successor_pane);
        let bound_successor_registration = successor_mux
            .add_tab_and_active_pane(&successor_tab)
            .unwrap()
            .unwrap();
        assert!(bound_successor_registration.same_registration(&successor_registration));
        let successor_window = successor_mux.new_empty_window(None, None);
        successor_mux
            .add_tab_to_window(&successor_tab, *successor_window)
            .unwrap();
        drop(successor_window);

        assert_eq!(std::fs::read(&births).unwrap(), b"B");
        assert!(!signaled.exists());
        assert!(is_child_alive(original_pid));

        // 6. Observe fresh child output from original child
        std::fs::write(&post_successor, b"step").unwrap();
        let post_successor_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            let (_, lines) = successor_pane.get_lines(0..24);
            // The fixture emits markers without newlines; the successor
            // marker crosses a physical row after the restored prefix.
            let mut observed = String::new();
            for line in &lines {
                observed.push_str(line.as_str().as_ref());
            }
            if observed.contains("guardian-domain-post-successor-marker") {
                break;
            }
            assert!(
                Instant::now() < post_successor_deadline,
                "successor pane did not render fresh child output after recovery: {:?}",
                lines.iter().map(|line| line.as_str()).collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(2));
        }

        // 7. Capture generation 2 checkpoint and build generation 2 image
        let gen2_published =
            capture_real_guardian_checkpoint(&successor_mux, successor_pane.pane_id(), executor);
        let gen2_provenance = successor_pane
            .guardian_spawn_custody()
            .expect("successor pane retains authenticated custody");
        assert_eq!(gen2_provenance.original, provenance.original);
        let gen2_selector = gen2_provenance
            .acknowledged_successor
            .expect("successful Claim retained its acknowledged successor custody");
        assert_eq!(gen2_selector.ack_id, claim_request_id);
        assert_eq!(gen2_selector.handoff_id, claim_effect_id);
        assert_eq!(gen2_selector.lease_generation, 2);
        assert_eq!(gen2_provenance.current_lease_generation, 2);
        assert_eq!(
            gen2_provenance.current_mux_incarnation,
            successor_mux_incarnation
        );

        let gen2_captured = successor_mux
            .capture_topology_coherent(Default::default())
            .unwrap();
        assert_eq!(gen2_captured.pane_bindings.len(), 1);
        assert_eq!(
            gen2_captured.pane_bindings[0].spawn_custody,
            Some(gen2_provenance)
        );

        // Successor stage: predecessor registration was detached; original mux MUST return false
        assert!(
            !mux.guardian_checkpoint_is_current(pane_id, &published),
            "predecessor registration was detached; must be stale on predecessor mux"
        );

        assert!(
            successor_mux.guardian_checkpoint_is_current(successor_pane.pane_id(), &gen2_published),
            "fresh real guardian capture must be current under exact successor registration"
        );

        let unknown_pane_id = alloc_pane_id().expect("allocate unknown pane id");
        assert!(
            !successor_mux.guardian_checkpoint_is_current(unknown_pane_id, &gen2_published),
            "unknown pane ID must reject guardian checkpoint"
        );

        // A real model-only capture must not acquire guardian authority merely
        // by carrying the current pane's registration identity.
        let model_checkpoint = Terminal::new(
            size,
            Arc::new(config::TermConfig::new()),
            "FrankenTerm",
            config::wezterm_version(),
            Box::new(io::sink()),
        )
        .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
        .unwrap();
        let model_state = TerminalCheckpointV2::decode_canonical_json(
            model_checkpoint.canonical_payload(),
            TerminalCheckpointLimits::default(),
        )
        .unwrap();
        let dummy_model_ack = mux::ModelParserCheckpointAck {
            registration_wire_identity: gen2_published.capture().registration_wire_identity(),
            durable_pane_id: gen2_published.capture().durable_pane_id(),
            parser_stream_bytes: model_checkpoint.parser_stream_bytes(),
            semantic_generation: model_state.checkpoint().semantic_generation(),
            terminal_checkpoint: model_checkpoint,
        };
        assert!(
            !successor_mux.model_checkpoint_is_current(successor_pane.pane_id(), &dummy_model_ack),
            "model checkpoint query must reject guardian pane"
        );

        assert!(
            !successor_mux.guardian_checkpoint_is_current(successor_pane.pane_id(), &published),
            "predecessor gen1 capture must not be current under successor mux (lease/incarnation mismatch)"
        );

        // Deterministic cancellation during live guardian wait blocked on live parser
        {
            let physical_top = successor_pane.get_dimensions().physical_top;
            let (acquired_tx, acquired_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            let blocking_pane = Arc::clone(&successor_pane);
            struct HoldTerminalLines {
                acquired_tx: Option<std::sync::mpsc::SyncSender<()>>,
                release_rx: std::sync::mpsc::Receiver<()>,
            }
            impl mux::pane::WithPaneLines for HoldTerminalLines {
                fn with_lines_mut(
                    &mut self,
                    _first: wezterm_term::StableRowIndex,
                    _lines: &mut [&mut termwiz::surface::Line],
                ) {
                    if let Some(tx) = self.acquired_tx.take() {
                        tx.send(()).expect("report held terminal lock");
                    }
                    self.release_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release terminal lock after bounded cancellation");
                }
            }
            let blocker = thread::spawn(move || {
                blocking_pane.with_lines_mut(
                    physical_top..physical_top.checked_add(1).unwrap(),
                    &mut HoldTerminalLines {
                        acquired_tx: Some(acquired_tx),
                        release_rx,
                    },
                );
            });
            acquired_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("blocker acquired the live terminal lock");
            let started = Instant::now();
            let cancel_result = successor_mux.capture_pane_guardian_checkpoint(
                successor_pane.pane_id(),
                TerminalCheckpointLimits::default(),
                Duration::from_secs(5),
                || started.elapsed() >= Duration::from_millis(250),
            );
            let cancel_elapsed = started.elapsed();
            release_tx.send(()).expect("release owned terminal blocker");
            blocker.join().expect("owned terminal blocker settled");
            assert!(
                cancel_elapsed >= Duration::from_millis(200),
                "cancellation must enforce elapsed lower-bound while waiting on parser, elapsed: {cancel_elapsed:?}"
            );
            assert!(
                cancel_elapsed < Duration::from_secs(2),
                "cancellation during wait must abort within bounded duration, elapsed: {cancel_elapsed:?}"
            );
            assert!(
                matches!(
                    cancel_result
                        .as_ref()
                        .err()
                        .and_then(|err| err.downcast_ref::<mux::LiveParserCheckpointError>()),
                    Some(mux::LiveParserCheckpointError::Cancelled)
                ),
                "expected LiveParserCheckpointError::Cancelled, got {cancel_result:?}"
            );
        }

        let gen2_object_id = "real-guardian-terminal-gen2";
        let gen2_timestamp = gen2_captured.captured_at_epoch_ms;
        let encode_gen2 = |payload: &[u8], id, kind, predecessor_generation: Option<u64>| {
            encode_recovery_object(
                payload,
                ObjectMetadata::single(id, kind, 2, predecessor_generation, gen2_timestamp),
                &key,
                None,
            )
            .unwrap()
            .to_bytes()
            .unwrap()
        };
        let gen2_ciphertext = encode_gen2(
            gen2_published
                .capture()
                .terminal_checkpoint()
                .canonical_payload(),
            semantic_object_id_from_str(gen2_object_id),
            RecoveryObjectKind::TerminalCheckpoint,
            None,
        );
        let gen2_digest: [u8; 32] = Sha256::digest(&gen2_ciphertext).into();
        let gen2_references = HashMap::from([(
            successor_pane.pane_id(),
            RecoveryObjectRef {
                object_id: gen2_object_id.into(),
                byte_length: gen2_ciphertext.len() as u64,
                payload_digest: gen2_digest,
                schema_version: 2,
            },
        )]);
        let gen2_parser_checkpoints = HashMap::from([(
            successor_pane.pane_id(),
            RecoveryParserCheckpoint::Guardian(&gen2_published),
        )]);
        let gen2_image = MuxRecoveryImage::from_mux_captured_checkpoints(
            RecoveryImageGenerationMeta {
                generation: 2,
                predecessor_digest: Some(image.image_digest),
                created_at_epoch_ms: gen2_timestamp,
                ft_version: config::wezterm_version().to_owned(),
                session_id: "real-guardian-birth".into(),
            },
            &gen2_captured,
            &gen2_parser_checkpoints,
            &gen2_references,
        )
        .unwrap();
        let gen2_image_bytes = gen2_image.to_canonical_json().unwrap();
        assert_eq!(
            MuxRecoveryImage::from_json_slice(&gen2_image_bytes).unwrap(),
            gen2_image
        );

        store
            .publish_object(&RecoveryObjectPayload {
                object_id: gen2_object_id.into(),
                expected_sha256: hex::encode(gen2_digest),
                ciphertext_bytes: gen2_ciphertext,
            })
            .unwrap();

        // Construct a fresh authorized verifier with existing guardian custody
        // and no volatile registrations so cross-generation checkpoints are authenticated
        // from durable catalog/ACK rather than triggering duplicate durable pane ID rejection.
        let authorized_verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::clone(&key),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        )
        .with_existing_guardian_custody(token.to_path_buf());

        // Re-encryption and a recomputed image digest cannot turn an arbitrary
        // selector into acknowledged custody. No failed candidate changes roots.
        for control in 0..7 {
            let mut changed = gen2_image.clone();
            let frankenterm_core::mux_recovery_image::RecoverySpawnCustody::Original {
                acknowledged_successor: Some(c),
                ..
            } = &mut changed.panes[0].spawn_custody
            else {
                panic!("gen2 custody selector");
            };
            match control {
                0 => c.ack_id = Uuid::new_v4(),
                1 => c.handoff_id = Uuid::new_v4(),
                2 => c.successor.connection_id = Uuid::new_v4(),
                3 => c.pane_id = Uuid::new_v4(),
                4 => c.lease_generation += 1,
                5 => c.successor.mux_build[0] ^= 1,
                _ => c.predecessor.connection_id = Uuid::new_v4(),
            }
            changed.image_digest = changed.compute_digest().unwrap();
            let root = GenerationRootPublishRequest {
                generation: 2,
                publisher_id: "altered-successor-selector".into(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: root_receipt.generation,
                    expected_hash: root_receipt.sha256.clone(),
                }),
                manifest_bytes: encode_gen2(
                    &serde_json::to_vec(&changed).unwrap(),
                    [0x72; 32],
                    RecoveryObjectKind::WholeMuxImage,
                    Some(1),
                ),
                created_at_ms: gen2_timestamp,
            };
            assert!(
                matches!(
                    store.publish_generation_root(&root, &authorized_verifier),
                    Err(PublicationError::VerificationRejected { generation: 2, .. })
                ),
                "altered successor selector control {control} must fail encrypted reopen"
            );
            assert_eq!(store.inspect_root_candidates().unwrap().0.len(), 1);
        }

        // Negative control 1: mismatched predecessor generation in manifest must be rejected by authorized verifier
        let bad_predecessor_root = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "successor-claim".into(),
            predecessor: Some(PredecessorBinding {
                expected_generation: root_receipt.generation,
                expected_hash: root_receipt.sha256.clone(),
            }),
            manifest_bytes: encode_gen2(
                &gen2_image_bytes,
                [0x72; 32],
                RecoveryObjectKind::WholeMuxImage,
                None,
            ),
            created_at_ms: gen2_timestamp,
        };
        match store.publish_generation_root(&bad_predecessor_root, &authorized_verifier) {
            Err(PublicationError::VerificationRejected { generation, .. }) => {
                assert_eq!(generation, 2);
            }
            other => panic!(
                "expected VerificationRejected for mismatched predecessor metadata, got {other:?}"
            ),
        }

        // Negative control 2: mismatched generation in manifest must be rejected by authorized verifier
        let bad_generation_manifest = encode_recovery_object(
            &gen2_image_bytes,
            ObjectMetadata::single(
                [0x72; 32],
                RecoveryObjectKind::WholeMuxImage,
                1,
                None,
                gen2_timestamp,
            ),
            &key,
            None,
        )
        .unwrap()
        .to_bytes()
        .unwrap();
        let bad_generation_root = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "successor-claim".into(),
            predecessor: Some(PredecessorBinding {
                expected_generation: root_receipt.generation,
                expected_hash: root_receipt.sha256.clone(),
            }),
            manifest_bytes: bad_generation_manifest,
            created_at_ms: gen2_timestamp,
        };
        match store.publish_generation_root(&bad_generation_root, &authorized_verifier) {
            Err(PublicationError::VerificationRejected { generation, .. }) => {
                assert_eq!(generation, 2);
            }
            other => panic!(
                "expected VerificationRejected for mismatched generation metadata, got {other:?}"
            ),
        }

        let gen2_root = GenerationRootPublishRequest {
            generation: 2,
            publisher_id: "successor-claim".into(),
            predecessor: Some(PredecessorBinding {
                expected_generation: root_receipt.generation,
                expected_hash: root_receipt.sha256.clone(),
            }),
            manifest_bytes: encode_gen2(
                &gen2_image_bytes,
                [0x72; 32],
                RecoveryObjectKind::WholeMuxImage,
                Some(1),
            ),
            created_at_ms: gen2_timestamp,
        };

        // Negative control 3: unregistered generation 2 capture rejected by original gen1-only verifier
        match store.publish_generation_root(&gen2_root, &verifier) {
            Err(PublicationError::VerificationRejected { generation, .. }) => {
                assert_eq!(generation, 2);
            }
            other => {
                panic!("expected VerificationRejected for unregistered gen2 capture, got {other:?}")
            }
        }

        let gen2_root_receipt = store
            .publish_generation_root(&gen2_root, &authorized_verifier)
            .unwrap();
        assert_eq!(gen2_root_receipt.generation, 2);

        // Successor Gen 2 whole-mux recovery capture in same store with exact predecessor hash & verified image digest
        {
            let recovery_expected_gen2 = WholeMuxPublicationIdentity {
                generation: 2,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(gen2_captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "successor-claim".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: 1,
                    expected_hash: recovery_gen1_hash,
                }),
                predecessor_image_digest: Some(recovery_gen1_image_digest),
                existing_guardian_custody: Some(token.to_path_buf()),
            };
            let recovery_gen2_receipt = {
                let thread_cx = frankenterm_core::cx::Cx::for_testing();
                let thread_mux = Arc::clone(&successor_mux);
                let thread_store = SnapshotPublicationStore::open(
                    directory.join("whole-image-common-cut"),
                    Default::default(),
                )
                .unwrap();
                let thread_key = Arc::clone(&key);
                let thread_expected = recovery_expected_gen2;
                let handle = thread::spawn(move || {
                    capture_and_publish_whole_mux_recovery(
                        &thread_cx,
                        &thread_mux,
                        &thread_store,
                        thread_key,
                        &thread_expected,
                        Duration::from_secs(5),
                    )
                });
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    while executor.try_tick().unwrap() {}
                    if handle.is_finished() {
                        break handle.join().unwrap().expect(
                            "capture and publish successor generation 2 in same recovery store",
                        );
                    }
                    assert!(Instant::now() < deadline, "gen2 recovery capture timed out");
                    thread::sleep(Duration::from_millis(2));
                }
            };
            assert_eq!(recovery_gen2_receipt.generation, 2);

            let recovery_gen2_selected = recovery_store
                .select_verified_roots(&authorized_verifier)
                .expect("authorized verifier must select verified generation 2 root");
            let recovery_gen2_validated = recovery_gen2_selected
                .current
                .expect("must verify published generation 2 root");
            assert_eq!(recovery_gen2_validated.generation(), 2);
            assert_eq!(
                recovery_gen2_validated.image().header.predecessor_digest,
                Some(recovery_gen1_image_digest)
            );
        }

        // 8. Fresh-process verification from disk without volatile per-pane hints
        let gen2_child_log_path = directory.join("fresh-image-verifier-gen2.stdout");
        let gen2_child_log = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gen2_child_log_path)
            .unwrap();
        let mut gen2_child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guardian_proxy::tests::guardian_image_fresh_process_existing_custody",
                "--nocapture",
            ])
            .env("FT_TEST_IMAGE_REOPEN_ROOT", directory.join("whole-image"))
            .env("FT_TEST_IMAGE_REOPEN_TOKEN", token)
            .env("FT_TEST_IMAGE_EXPECTED_GENERATION", "2")
            .env(
                "FT_TEST_IMAGE_EXPECTED_MARKER",
                "guardian-domain-post-successor-marker",
            )
            .env_remove("FT_TEST_IMAGE_REOPEN_SCOPE")
            .env_remove("FT_TEST_IMAGE_REOPEN_CHECKPOINT")
            .stdout(gen2_child_log)
            .spawn()
            .unwrap();
        let gen2_child_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = gen2_child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fresh process could not authenticate durable generation 2 image"
                );
                break;
            }
            if Instant::now() >= gen2_child_deadline {
                gen2_child
                    .kill()
                    .expect("settle only owned image verifier child");
                gen2_child.wait().unwrap();
                panic!("fresh process generation 2 image verifier deadline expired");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let gen2_child_log = std::fs::read_to_string(gen2_child_log_path).unwrap();
        assert!(
            gen2_child_log.contains("running 1 test")
                && gen2_child_log.contains("1 passed; 0 failed"),
            "fresh gen2 verifier did not execute exactly one test: {}",
            gen2_child_log
        );

        let validated_gen2 = store
            .select_verified_roots(&authorized_verifier)
            .unwrap()
            .current
            .unwrap();
        assert_eq!(validated_gen2.generation(), 2);
        assert_eq!(validated_gen2.image().image_digest, gen2_image.image_digest);
        assert_eq!(
            validated_gen2.image().header.predecessor_digest,
            Some(image.image_digest)
        );

        let restored_gen2 = reconstruct_whole_mux_image_inert(
            &validated_gen2,
            TerminalCheckpointLimits::default(),
            None,
            &std::collections::HashSet::<String>::new(),
        )
        .unwrap();
        let restored_gen2_pane = &restored_gen2.pane_terminals[&(successor_pane.pane_id() as u64)];
        assert_eq!(
            restored_gen2_pane.spawn_custody,
            gen2_image.panes[0].spawn_custody
        );
        let restored_gen2_canonical = restored_gen2_pane
            .terminal
            .checkpoint()
            .unwrap()
            .to_canonical_json(TerminalCheckpointLimits::default())
            .unwrap();
        assert_eq!(
            restored_gen2_canonical.as_slice(),
            gen2_published
                .capture()
                .terminal_checkpoint()
                .canonical_payload()
        );
        let gen2_checkpoint_json: serde_json::Value =
            serde_json::from_slice(&restored_gen2_canonical).unwrap();
        let restored_gen2_text: String = gen2_checkpoint_json["primary_screen"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|line| line["cells"].as_array().unwrap())
            .map(|cell| cell["text"].as_str().unwrap())
            .collect();
        assert!(
            restored_gen2_text.contains("guardian-domain-post-successor-marker"),
            "generation 2 reconstructed terminal did not contain post-successor child output"
        );

        assert_eq!(std::fs::read(&births).unwrap(), b"B");
        assert!(!signaled.exists());
        assert!(is_child_alive(original_pid));

        // Production whole-mux publisher integration: publish Gen 1 and Gen 2 across real mux replacement.
        {
            use frankenterm_core::snapshot_engine::{
                WholeMuxPanePublication, WholeMuxPublicationError, WholeMuxPublicationIdentity,
                publish_whole_mux_recovery,
            };
            let prod_store = SnapshotPublicationStore::open(
                directory.join("whole-image-prod-pipeline"),
                Default::default(),
            )
            .unwrap();
            let prod_cx = frankenterm_core::cx::for_testing();
            let prod_expected_gen1 = WholeMuxPublicationIdentity {
                generation: 1,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "actual-birth".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: None,
                predecessor_image_digest: None,
                existing_guardian_custody: None,
            };
            let prod_pub_gen1 = WholeMuxPanePublication::guardian(
                pane_id,
                "prod-guardian-terminal-gen1",
                &published,
            );
            let prod_receipt_gen1 = publish_whole_mux_recovery(
                &prod_cx,
                &prod_store,
                &captured,
                &[prod_pub_gen1],
                Arc::clone(&key),
                &prod_expected_gen1,
            )
            .expect("production publisher must publish generation 1 guardian capture");
            assert_eq!(prod_receipt_gen1.generation, 1);
            assert_eq!(prod_store.inspect_root_candidates().unwrap().0.len(), 1);

            // Obtain actual production gen1 verified image/digest from production root using exact enrolled verifier.
            let prod_gen1_selected = prod_store
                .select_verified_roots(&verifier)
                .expect("exact enrolled verifier must select verified generation 1 root");
            let prod_gen1_validated = prod_gen1_selected
                .current
                .expect("must verify published generation 1 root");
            let prod_gen1_image_digest = prod_gen1_validated.image().image_digest;

            let prod_pub_gen2 = WholeMuxPanePublication::guardian(
                successor_pane.pane_id(),
                "prod-guardian-terminal-gen2",
                &gen2_published,
            );

            // Negative control 1: Gen 2 without predecessor custody token rejects predecessor.
            let prod_expected_gen2_no_custody = WholeMuxPublicationIdentity {
                generation: 2,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(gen2_captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "successor-claim".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: 1,
                    expected_hash: prod_receipt_gen1.sha256.clone(),
                }),
                predecessor_image_digest: Some(prod_gen1_image_digest),
                existing_guardian_custody: None,
            };
            let prod_pub_gen2_missing = WholeMuxPanePublication::guardian(
                successor_pane.pane_id(),
                "prod-guardian-terminal-gen2-missing",
                &gen2_published,
            );
            let err = publish_whole_mux_recovery(
                &prod_cx,
                &prod_store,
                &gen2_captured,
                &[prod_pub_gen2_missing],
                Arc::clone(&key),
                &prod_expected_gen2_no_custody,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WholeMuxPublicationError::Publication(PublicationError::InvalidDiscovery(
                        "committed graph rejected"
                    ))
                ),
                "publication without predecessor custody must fail predecessor reconciliation: {err:?}"
            );
            assert_eq!(prod_store.inspect_root_candidates().unwrap().0.len(), 1);

            // Custody belongs to the authenticated store in the token's parent
            // directory, so another token basename in the real directory is
            // still valid custody. A missing store must reject the predecessor.
            let prod_expected_gen2_wrong_custody = WholeMuxPublicationIdentity {
                generation: 2,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(gen2_captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "successor-claim".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: 1,
                    expected_hash: prod_receipt_gen1.sha256.clone(),
                }),
                predecessor_image_digest: Some(prod_gen1_image_digest),
                existing_guardian_custody: Some(
                    directory
                        .join("nonexistent-custody-directory")
                        .join("guardian.token"),
                ),
            };
            let prod_pub_gen2_wrong = WholeMuxPanePublication::guardian(
                successor_pane.pane_id(),
                "prod-guardian-terminal-gen2-wrong",
                &gen2_published,
            );
            let err = publish_whole_mux_recovery(
                &prod_cx,
                &prod_store,
                &gen2_captured,
                &[prod_pub_gen2_wrong],
                Arc::clone(&key),
                &prod_expected_gen2_wrong_custody,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WholeMuxPublicationError::Publication(PublicationError::InvalidDiscovery(
                        "committed graph rejected"
                    ))
                ),
                "publication with wrong predecessor custody must fail predecessor reconciliation: {err:?}"
            );
            assert_eq!(prod_store.inspect_root_candidates().unwrap().0.len(), 1);

            // Negative control 3: Gen 2 with mismatched predecessor image digest rejects predecessor.
            let mut wrong_digest = prod_gen1_image_digest;
            wrong_digest[0] ^= 0xff;
            let prod_expected_gen2_wrong_digest = WholeMuxPublicationIdentity {
                generation: 2,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(gen2_captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "successor-claim".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: 1,
                    expected_hash: prod_receipt_gen1.sha256.clone(),
                }),
                predecessor_image_digest: Some(wrong_digest),
                existing_guardian_custody: Some(token.to_path_buf()),
            };
            let prod_pub_gen2_wrong_digest = WholeMuxPanePublication::guardian(
                successor_pane.pane_id(),
                "prod-guardian-terminal-gen2-wrong-digest",
                &gen2_published,
            );
            let err = publish_whole_mux_recovery(
                &prod_cx,
                &prod_store,
                &gen2_captured,
                &[prod_pub_gen2_wrong_digest],
                Arc::clone(&key),
                &prod_expected_gen2_wrong_digest,
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WholeMuxPublicationError::Publication(PublicationError::InvalidDiscovery(
                        "committed graph rejected"
                    ))
                ),
                "publication with wrong predecessor image digest must fail predecessor reconciliation: {err:?}"
            );
            assert_eq!(prod_store.inspect_root_candidates().unwrap().0.len(), 1);

            // Successful Gen 2 publication across actual mux replacement with caller-enrolled custody token.
            let prod_expected_gen2 = WholeMuxPublicationIdentity {
                generation: 2,
                session_id: "real-guardian-birth".into(),
                mux_incarnation_id: hex::encode(gen2_captured.session_incarnation.as_bytes()),
                root_object_id: [0x72; 32],
                publisher_id: "successor-claim".into(),
                ft_version: config::wezterm_version().to_owned(),
                predecessor: Some(PredecessorBinding {
                    expected_generation: 1,
                    expected_hash: prod_receipt_gen1.sha256.clone(),
                }),
                predecessor_image_digest: Some(prod_gen1_image_digest),
                existing_guardian_custody: Some(token.to_path_buf()),
            };
            let prod_receipt_gen2 = publish_whole_mux_recovery(
                &prod_cx,
                &prod_store,
                &gen2_captured,
                &[prod_pub_gen2],
                Arc::clone(&key),
                &prod_expected_gen2,
            )
            .expect("production publication of generation 2 across successor mux must succeed");
            assert_eq!(prod_receipt_gen2.generation, 2);
            assert!(
                prod_store
                    .has_object("prod-guardian-terminal-gen2")
                    .unwrap()
            );
            assert_eq!(prod_store.inspect_root_candidates().unwrap().0.len(), 2);

            let prod_selected = prod_store
                .select_verified_roots(&authorized_verifier)
                .expect("authorized verifier must select verified generation 2 root");
            let prod_validated_gen2 = prod_selected
                .current
                .expect("must verify published generation 2 root");
            assert_eq!(prod_validated_gen2.generation(), 2);
            assert_eq!(
                prod_validated_gen2.image().header.predecessor_digest,
                Some(prod_gen1_image_digest)
            );
            assert_eq!(
                prod_validated_gen2.image().header.mux_incarnation_id,
                hex::encode(gen2_captured.session_incarnation.as_bytes())
            );
        }

        // Terminal mutation with same stream bytes advances semantic generation; bound capture witness must be rejected
        successor_pane.perform_actions(vec![termwiz::escape::Action::Print('!')]);
        assert!(
            !successor_mux
                .guardian_checkpoint_is_current(successor_pane.pane_id(), &gen2_published),
            "terminal mutation advanced semantic generation; bound capture witness must be rejected"
        );

        // Restore the independently reopened generation-2 image into a third
        // mux. Its selector must reach the negotiated successor Claim path.
        let third_mux = Arc::new(Mux::new(None));
        let (third_session, _) = third_mux.topology_snapshot_authority().unwrap();
        let third_incarnation = Uuid::from_bytes(third_session.as_bytes());
        let third_census = Arc::new(
            GuardianCensusCoordinator::connect(
                socket,
                token,
                provenance.original.guardian_incarnation,
                third_incarnation,
            )
            .unwrap(),
        );
        let prepare_third = || {
            GuardianProxyLeasePlan::prepare_from_recovery(
                socket,
                token,
                PtySize {
                    rows: size.rows as u16,
                    cols: size.cols as u16,
                    pixel_width: size.pixel_width as u16,
                    pixel_height: size.pixel_height as u16,
                },
                Arc::clone(&third_census),
                &validated_gen2,
                provenance.original.pane_id,
            )
            .unwrap()
        };
        assert!(
            matches!(
                prepare_third().claim(
                    provenance.original.pane_id,
                    2,
                    Uuid::new_v4(),
                    Uuid::new_v4()
                ),
                Err(GuardianProxyError::Client(GuardianClientError::Rejected(
                    GuardianRejectionCode::InvalidRequest
                )))
            ),
            "selected successor custody must not displace a living mux owner"
        );

        assert!(successor_registration.detach_local_if_current());
        drop(successor_registration);
        drop(bound_successor_registration);
        assert!(successor_mux.remove_tab_local_only_if_same(&successor_tab));
        assert!(successor_mux.domain_was_detached_if_same(&registered_successor_domain));
        drop(successor_tab);
        drop(successor_pane);
        drop(successor_coordinator);
        drop(successor_observer);
        drop(registered_successor_domain);
        drop(successor_domain);
        while executor.try_tick().unwrap() {}

        let third_request = Uuid::new_v4();
        let third_handoff = Uuid::new_v4();
        let deadline = Instant::now() + Duration::from_secs(5);
        let third_staging = loop {
            while executor.try_tick().unwrap() {}
            match prepare_third().claim(
                provenance.original.pane_id,
                2,
                third_request,
                third_handoff,
            ) {
                Ok(staging) => break staging,
                Err(GuardianProxyError::Client(GuardianClientError::Rejected(
                    GuardianRejectionCode::InvalidRequest,
                ))) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
                Err(error) => panic!("second image-backed successor Claim failed: {error}"),
            }
        };
        let third_domain: Arc<dyn Domain> = Arc::new(
            GuardianDomain::new(&third_mux, socket.to_path_buf(), token.to_path_buf()).unwrap(),
        );
        third_mux.add_domain(&third_domain).unwrap();
        third_mux.set_default_domain(&third_domain).unwrap();
        let third_pane_id = alloc_pane_id().unwrap();
        let description = "second image-backed successor".to_owned();
        let third_config: Arc<dyn TerminalConfiguration> =
            Arc::new(config::TermConfig::new_for_pane(
                third_pane_id,
                third_domain.domain_id(),
                *provenance.original.pane_id.as_bytes(),
                description.clone(),
            ));
        let third_pane = mux::domain::UnpublishedPane::from_guardian_proxy(
            third_staging
                .restore_and_activate(third_config, TerminalCheckpointLimits::default())
                .unwrap()
                .into_local_pane(third_pane_id, third_domain.domain_id(), description),
        )
        .unwrap()
        .publish(&third_mux)
        .unwrap();
        let third_tab = Arc::new(mux::tab::Tab::new(&size));
        third_tab.assign_pane(&third_pane);
        third_mux.add_tab_and_active_pane(&third_tab).unwrap();
        let window = third_mux.new_empty_window(None, None);
        third_mux.add_tab_to_window(&third_tab, *window).unwrap();
        drop(window);
        let third_provenance = third_pane.guardian_spawn_custody().unwrap();
        assert_eq!(third_provenance.original, provenance.original);
        assert_eq!(third_provenance.current_lease_generation, 3);
        let third_selector = third_provenance.acknowledged_successor.unwrap();
        assert_eq!(third_selector.ack_id, third_request);
        assert_eq!(third_selector.handoff_id, third_handoff);
        assert_eq!(third_selector.predecessor, gen2_selector.successor);
        assert_eq!(std::fs::read(&births).unwrap(), b"B");
        assert!(!signaled.exists());
        assert!(is_child_alive(original_pid));

        std::fs::write(directory.join("post-second-successor"), b"step").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while executor.try_tick().unwrap() {}
            let (_, lines) = third_pane.get_lines(0..24);
            let text: String = lines.iter().map(|line| line.as_str().to_string()).collect();
            if text.contains("guardian-domain-second-successor-marker") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "second successor did not render fresh original-child output"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let gen3_published = capture_real_guardian_checkpoint(&third_mux, third_pane_id, executor);
        let gen3_captured = third_mux
            .capture_topology_coherent(Default::default())
            .unwrap();
        assert_eq!(
            gen3_captured.pane_bindings[0].spawn_custody,
            Some(third_provenance)
        );
        let gen3_identity = WholeMuxPublicationIdentity {
            generation: 3,
            session_id: "real-guardian-birth".into(),
            mux_incarnation_id: hex::encode(third_session.as_bytes()),
            root_object_id: [0x72; 32],
            publisher_id: "second-successor-claim".into(),
            ft_version: config::wezterm_version().to_owned(),
            predecessor: Some(PredecessorBinding {
                expected_generation: 2,
                expected_hash: gen2_root_receipt.sha256,
            }),
            predecessor_image_digest: Some(gen2_image.image_digest),
            existing_guardian_custody: Some(token.to_path_buf()),
        };
        frankenterm_core::snapshot_engine::publish_whole_mux_recovery(
            &frankenterm_core::cx::for_testing(),
            &store,
            &gen3_captured,
            &[
                frankenterm_core::snapshot_engine::WholeMuxPanePublication::guardian(
                    third_pane_id,
                    "real-guardian-terminal-gen3",
                    &gen3_published,
                ),
            ],
            Arc::clone(&key),
            &gen3_identity,
        )
        .unwrap();
        let reopened = store
            .select_verified_roots(&authorized_verifier)
            .unwrap()
            .current
            .unwrap();
        assert_eq!(reopened.generation(), 3);
        assert_eq!(
            reopened.image().panes[0].spawn_custody,
            frankenterm_core::mux_recovery_image::RecoverySpawnCustody::from(Some(
                third_provenance
            ))
        );
        let child_log_path = directory.join("fresh-image-verifier-gen3.stdout");
        let child_log = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&child_log_path)
            .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guardian_proxy::tests::guardian_image_fresh_process_existing_custody",
                "--nocapture",
            ])
            .env("FT_TEST_IMAGE_REOPEN_ROOT", directory.join("whole-image"))
            .env("FT_TEST_IMAGE_REOPEN_TOKEN", token)
            .env("FT_TEST_IMAGE_EXPECTED_GENERATION", "3")
            .env(
                "FT_TEST_IMAGE_EXPECTED_MARKER",
                "guardian-domain-second-successor-marker",
            )
            .env_remove("FT_TEST_IMAGE_REOPEN_SCOPE")
            .env_remove("FT_TEST_IMAGE_REOPEN_CHECKPOINT")
            .stdout(child_log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fresh process rejected generation 3 custody/image"
                );
                break;
            }
            if Instant::now() >= deadline {
                child.kill().expect("settle owned image verifier child");
                child.wait().unwrap();
                panic!("fresh generation 3 verifier deadline expired");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let child_log = std::fs::read_to_string(child_log_path).unwrap();
        assert!(
            child_log.contains("running 1 test") && child_log.contains("1 passed; 0 failed"),
            "fresh generation 3 verifier did not execute its test: {child_log}"
        );
        (third_mux, third_pane)
    }

    #[test]
    fn guardian_image_fresh_process_existing_custody() {
        let Some(root) = std::env::var_os("FT_TEST_IMAGE_REOPEN_ROOT") else {
            return;
        };
        use frankenterm_core::session_restore::{
            WholeMuxRecoveryVerifier, WholeMuxTrustedIdentityConfig,
        };
        use frankenterm_core::snapshot_publication::SnapshotPublicationStore;
        use frankenterm_core::snapshot_representation::RecoveryKey;
        let token = PathBuf::from(std::env::var_os("FT_TEST_IMAGE_REOPEN_TOKEN").unwrap());
        assert!(std::env::var_os("FT_TEST_IMAGE_REOPEN_SCOPE").is_none());
        assert!(std::env::var_os("FT_TEST_IMAGE_REOPEN_CHECKPOINT").is_none());
        let store =
            SnapshotPublicationStore::open(PathBuf::from(root), Default::default()).unwrap();
        let verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::new(RecoveryKey::from_bytes([0x51; 32]).unwrap()),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        );
        assert!(
            store
                .select_verified_roots(&verifier)
                .unwrap()
                .current
                .is_none()
        );
        let missing_directory = token.parent().unwrap().join("missing-custody");
        let missing_verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::new(RecoveryKey::from_bytes([0x51; 32]).unwrap()),
            WholeMuxTrustedIdentityConfig::new([0x72; 32]),
        )
        .with_existing_guardian_custody(missing_directory.join("guardian.token"));
        assert!(
            store
                .select_verified_roots(&missing_verifier)
                .unwrap()
                .current
                .is_none()
        );
        assert!(
            !missing_directory.exists(),
            "verification must not initialize missing custody"
        );
        let verifier = verifier.with_existing_guardian_custody(token);
        let selected = store.select_verified_roots(&verifier).unwrap();
        assert!(
            selected.current.is_some(),
            "durable custody/catalog/ACK did not verify saved image: {:?}",
            selected.torn_or_rejected
        );
        if let Some(expected) = std::env::var_os("FT_TEST_IMAGE_EXPECTED_GENERATION") {
            let expected_gen: u64 = expected.to_str().unwrap().parse().unwrap();
            assert_eq!(
                selected.current.as_ref().unwrap().generation(),
                expected_gen,
                "fresh process verified root generation mismatch"
            );
        }
        if let Some(expected_marker) = std::env::var_os("FT_TEST_IMAGE_EXPECTED_MARKER") {
            let marker_str = expected_marker.to_str().unwrap();
            use frankenterm_core::session_restore::reconstruct_whole_mux_image_inert;
            use wezterm_term::terminalstate::checkpoint::TerminalCheckpointLimits;
            let restored = reconstruct_whole_mux_image_inert(
                selected.current.as_ref().unwrap(),
                TerminalCheckpointLimits::default(),
                None,
                &std::collections::HashSet::<String>::new(),
            )
            .unwrap();
            let mut found_marker = false;
            for pane in restored.pane_terminals.values() {
                let canonical = pane
                    .terminal
                    .checkpoint()
                    .unwrap()
                    .to_canonical_json(TerminalCheckpointLimits::default())
                    .unwrap();
                let json: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
                let text: String = json["primary_screen"]["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|line| line["cells"].as_array().unwrap())
                    .map(|cell| cell["text"].as_str().unwrap())
                    .collect();
                if text.contains(marker_str) {
                    found_marker = true;
                    break;
                }
            }
            assert!(
                found_marker,
                "fresh process reconstructed terminal did not contain expected marker: {marker_str}"
            );
        }
    }

    struct RecordCheckpointFixture {
        capture: LiveParserCheckpointAck,
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

        fn checkpoint(
            &mut self,
            _pane_id: Uuid,
            _generation: u64,
            sequence: u64,
            request_id: Uuid,
            effect_id: Uuid,
            intent: GuardianCheckpointIntent,
        ) -> Result<GuardianCheckpointReceipt, GuardianMutationTransportError> {
            let _directive = self.record(FakeCall::Checkpoint {
                sequence,
                request_id,
                effect_id,
                intent,
            });
            Err(GuardianMutationTransportError::Client(
                GuardianClientError::UnexpectedReply,
            ))
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
        let (delivered_sender, delivered_receiver) = sync_channel(1);
        let (staging, mutation_state) = fake_staging([], 1);
        let (genesis, canonical) = genesis_checkpoint_fixture();
        let restored = GuardianRestoredTerminal::from_checkpoint(
            identity().pane_id(),
            genesis,
            &canonical,
            (640, 384),
            test_terminal_config(),
            TerminalCheckpointLimits::default(),
        )
        .unwrap();
        let activated = staging
            .activate_verified_restore(
                restored,
                Box::new(ChannelGuardianReader {
                    receiver,
                    delivered: delivered_sender,
                }),
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
        delivered_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("observe checkpoint fixture delivery authorization");
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
            capture,
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
        checkpoint_and_complete_pages_for_identity(descriptor, checkpoint, identity())
    }

    fn checkpoint_and_complete_pages_for_identity(
        descriptor: GuardianCheckpointDescriptorV1,
        checkpoint: Zeroizing<Vec<u8>>,
        page_identity: GuardianPaneLeaseIdentity,
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
            page_identity.pane_id(),
            page_identity.generation(),
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
        let base = GuardianReplayBoundary::from_descriptor(descriptor)
            .expect("fixture has a canonical replay boundary");
        let complete_page = GuardianReplayPageDelivery::new(
            page_identity.pane_id(),
            page_identity.generation(),
            snapshot_id,
            snapshot_digest,
            cursor.digest(),
            1,
            None,
            GuardianReplayPageBodyDelivery::Complete {
                checkpoint_id: descriptor.checkpoint_id(),
                through_sequence: base.through_sequence().unwrap(),
                terminal_record_digest: base.previous_record_digest,
                cumulative_plaintext_bytes: base.cumulative_plaintext_bytes,
            },
        )
        .expect("construct checkpoint fixture completion page");
        VecDeque::from([checkpoint_page, complete_page])
    }

    fn tail_output_page(
        fixture: &RecordCheckpointFixture,
        payload: &[u8],
    ) -> GuardianReplayPageDelivery {
        tail_output_page_records(fixture, &[payload])
    }

    fn tail_output_page_records(
        fixture: &RecordCheckpointFixture,
        payloads: &[&[u8]],
    ) -> GuardianReplayPageDelivery {
        assert!(!payloads.is_empty(), "tail page fixture needs a record");
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
        let first_sequence = sequence.checked_add(1).expect("tail sequence advances");
        let mut output_sequence = sequence;
        let mut output_cumulative = cumulative_plaintext_bytes;
        let mut output_log_bytes = committed_log_bytes;
        let mut output_digest = record_digest;
        let mut record_deliveries = Vec::new();
        record_deliveries
            .try_reserve_exact(payloads.len())
            .expect("bounded tail record fixture allocation");
        for payload in payloads {
            output_sequence = output_sequence
                .checked_add(1)
                .expect("tail sequence advances");
            output_cumulative = output_cumulative
                .checked_add(u64::try_from(payload.len()).expect("tail payload length fits u64"))
                .expect("tail cumulative bytes advance");
            output_log_bytes = output_log_bytes
                .checked_add(u64::try_from(payload.len()).expect("tail payload length fits u64"))
                .and_then(|bytes| bytes.checked_add(256))
                .expect("tail committed bytes advance");
            output_digest = Sha256::digest(payload).into();
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
            record_deliveries.push(
                GuardianReplayRecordDelivery::new(metadata, Zeroizing::new(payload.to_vec()))
                    .expect("construct tail record delivery"),
            );
        }
        let records = GuardianReplayOutputRecordsDelivery::new(
            first_sequence,
            record_digest,
            record_deliveries,
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
            | FakeCall::Checkpoint {
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
    fn checkpoint_stage_lost_replies_query_exactly_without_duplicate_committed_chunks() {
        let fixture = capture_record_checkpoint_fixture();
        let descriptor = fixture.descriptor;
        let expected_payload_bytes = fixture.checkpoint.len();
        let expected_payload_digest =
            <[u8; 32]>::from(Sha256::digest(fixture.checkpoint.as_slice()));
        let total_chunks = u32::try_from(
            descriptor
                .total_bytes()
                .div_ceil(u64::from(GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES)),
        )
        .expect("bounded fixture chunk count fits u32");
        let chunk_request_ids = (0..total_chunks)
            .map(|index| id(0x1_000 + u128::from(index)))
            .collect::<Vec<_>>();
        let query_request_id = id(0x1_000_000);
        let mut pending = PendingGuardianCheckpointPublication {
            scope: GuardianCheckpointScopeV1::Pane {
                pane_id: identity().pane_id(),
                generation: identity().generation(),
            },
            upload_id: id(0x1_000_001),
            descriptor,
            chunk_bytes: GUARDIAN_CHECKPOINT_STAGE_CHUNK_BYTES,
            total_chunks,
            capture: fixture.capture,
            begin_request_id: id(0x1_000_002),
            chunk_request_ids,
            query_request_id,
            seal_request_id: id(0x1_000_003),
            ack_request_id: id(0x1_000_004),
            completion_id: None,
            adoption_receipt: None,
        };
        let state = Arc::new(Mutex::new(FakeCheckpointStageState::new([
            GuardianCheckpointStageKindV1::Query,
            GuardianCheckpointStageKindV1::Begin,
            GuardianCheckpointStageKindV1::Chunk,
            GuardianCheckpointStageKindV1::Seal,
            GuardianCheckpointStageKindV1::Ack,
        ])));
        let mut transport = FakeCheckpointStageTransport {
            state: Arc::clone(&state),
        };

        let completion_id = pending
            .drive_to_sealed(&mut transport)
            .expect("lost Stage replies reconcile to the exact sealed upload");
        pending
            .drive_ack(&mut transport, completion_id)
            .expect("lost Stage Ack reply reconciles to durable Acked state");

        let state = state.lock();
        assert!(state.acked);
        assert_eq!(state.completion_id, Some(completion_id));
        assert_eq!(state.payload.len(), expected_payload_bytes);
        assert_eq!(
            <[u8; 32]>::from(Sha256::digest(state.payload.as_slice())),
            expected_payload_digest
        );
        assert_eq!(state.lose_reply_once, Vec::new());

        let query_ids = state
            .calls
            .iter()
            .filter_map(|(request_id, kind)| {
                (*kind == GuardianCheckpointStageKindV1::Query).then_some(*request_id)
            })
            .collect::<Vec<_>>();
        assert!(query_ids.len() >= 6, "every ambiguous phase is queried");
        assert!(
            query_ids
                .iter()
                .all(|request_id| *request_id == query_request_id),
            "all lost-reply recovery reuses one retained Query identity"
        );

        for (kind, expected_calls) in [
            (GuardianCheckpointStageKindV1::Begin, 1_usize),
            (
                GuardianCheckpointStageKindV1::Chunk,
                usize::try_from(total_chunks).expect("bounded fixture chunks fit usize"),
            ),
            (GuardianCheckpointStageKindV1::Seal, 1),
            (GuardianCheckpointStageKindV1::Ack, 1),
        ] {
            assert_eq!(
                state
                    .calls
                    .iter()
                    .filter(|(_, observed)| *observed == kind)
                    .count(),
                expected_calls,
                "a committed {kind:?} is discovered by Query, not retransmitted"
            );
        }

        let mut mutation_request_ids = state
            .calls
            .iter()
            .filter_map(|(request_id, kind)| {
                (*kind != GuardianCheckpointStageKindV1::Query).then_some(*request_id)
            })
            .collect::<Vec<_>>();
        mutation_request_ids.sort_unstable();
        mutation_request_ids.dedup();
        assert_eq!(
            mutation_request_ids.len(),
            usize::try_from(total_chunks)
                .expect("bounded fixture chunks fit usize")
                .checked_add(3)
                .expect("bounded mutation call count"),
            "every Stage mutation retains a distinct fixed request identity"
        );
    }

    #[test]
    fn selected_recovery_checkpoint_binds_exact_replay_before_acknowledgement() {
        let fixture = capture_record_checkpoint_fixture();
        let selected = SelectedRecoveryCheckpoint {
            pane_id: identity().pane_id(),
            generation: fixture.descriptor.capture_generation(),
            checkpoint_id: fixture.descriptor.checkpoint_id(),
            boundary_id: fixture.descriptor.boundary_id().into_bytes(),
            payload_digest: fixture.descriptor.terminal_payload_digest(),
            payload_bytes: fixture.descriptor.total_bytes(),
        };
        for control in 0..7 {
            let mut expected = selected;
            match control {
                0 => {}
                1 => {
                    expected.checkpoint_id =
                        GuardianCheckpointIdentityDigest::from_bytes([0x82; 32]).unwrap();
                }
                2 => expected.boundary_id = [0x83; 32],
                3 => expected.payload_digest = [0x84; 32],
                4 => expected.payload_bytes += 1,
                5 => expected.generation += 1,
                6 => expected.pane_id = id(0x85),
                _ => unreachable!(),
            }
            let state = Arc::new(Mutex::new(FakeReplayState {
                pages: checkpoint_and_complete_pages(
                    fixture.descriptor,
                    fixture.checkpoint.clone(),
                ),
                replay_io_failures: 0,
                ack_io_failures: 0,
                requests: Vec::new(),
                acks: Vec::new(),
            }));
            let mut transport = FakeReplayTransport {
                state: Arc::clone(&state),
            };
            let result = consume_one_guardian_replay_snapshot(
                &mut transport,
                identity(),
                size(24, 80),
                test_terminal_config(),
                TerminalCheckpointLimits::default(),
                Some(expected),
            );
            let state = state.lock();
            assert!(matches!(
                state.requests.first(),
                Some((_, GuardianReplayRequestV1::Open {
                    selector: GuardianReplaySelectorV1::ExactCheckpoint { checkpoint_id },
                    ..
                })) if *checkpoint_id == expected.checkpoint_id
            ));
            if control == 0 {
                let restored = result.expect("exact selected checkpoint must restore");
                assert_eq!(restored.checkpoint_id, selected.checkpoint_id);
                assert_eq!(state.acks.len(), 2);
            } else {
                assert!(
                    matches!(result, Err(GuardianProxyError::ReplayInvariant(_))),
                    "control {control} must reject a different selected checkpoint"
                );
                assert!(
                    state.acks.is_empty(),
                    "control {control} acknowledged wrong cut"
                );
            }
        }
        assert!(
            selected
                .validate_attach(selected.pane_id, selected.generation + 1)
                .is_ok()
        );
        assert!(
            selected
                .validate_attach(id(0x86), selected.generation + 1)
                .is_err()
        );
        assert!(
            selected
                .validate_attach(selected.pane_id, selected.generation)
                .is_err()
        );
        let exhausted = SelectedRecoveryCheckpoint {
            generation: u64::MAX,
            ..selected
        };
        assert!(exhausted.validate_attach(selected.pane_id, 0).is_err());
    }

    fn genesis_checkpoint_fixture() -> (GuardianCheckpointDescriptorV1, Zeroizing<Vec<u8>>) {
        let terminal = Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            test_terminal_config(),
            "FrankenTerm",
            "guardian-genesis-replay-test",
            Box::new(Vec::<u8>::new()),
        );
        let checkpoint = terminal
            .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
            .unwrap();
        let descriptor =
            GuardianCheckpointDescriptorV1::for_genesis_artifact(id(0x812), &checkpoint).unwrap();
        (descriptor, checkpoint.into_canonical_payload())
    }

    #[test]
    fn genesis_restore_consumes_canonical_zero_origin_after_page_identity_validation() {
        let (descriptor, checkpoint) = genesis_checkpoint_fixture();
        assert_eq!(descriptor.durable_pane_id(), None);
        assert_eq!(
            descriptor.capture_generation(),
            mux::guardian_protocol::GUARDIAN_GENESIS_CAPTURE_GENERATION
        );
        let state = Arc::new(Mutex::new(FakeReplayState {
            pages: checkpoint_and_complete_pages(descriptor, checkpoint),
            replay_io_failures: 0,
            ack_io_failures: 0,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut transport = FakeReplayTransport {
            state: Arc::clone(&state),
        };
        let restored = consume_one_guardian_replay_snapshot(
            &mut transport,
            identity(),
            size(24, 80),
            test_terminal_config(),
            TerminalCheckpointLimits::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            restored.boundary,
            GuardianReplayBoundary {
                next_sequence: 1,
                previous_record_digest: [0; 32],
                cumulative_plaintext_bytes: 0
            }
        );
        assert_eq!(restored.checkpoint_id, descriptor.checkpoint_id());
        restored.inert_terminal.checkpoint().unwrap();
        let state = state.lock();
        assert_eq!(state.acks.len(), 2);
        assert!(
            state
                .acks
                .iter()
                .all(|(_, ack)| ack.through_sequence() == 0)
        );
    }

    #[test]
    fn genesis_restore_rejects_foreign_page_identity_geometry_and_forged_terminal_origin() {
        for wrong_identity in [
            identity_for(id(0x813), identity().generation()),
            identity_for(identity().pane_id(), identity().generation() + 1),
        ] {
            let (descriptor, checkpoint) = genesis_checkpoint_fixture();
            let state = Arc::new(Mutex::new(FakeReplayState {
                pages: checkpoint_and_complete_pages_for_identity(
                    descriptor,
                    checkpoint,
                    wrong_identity,
                ),
                replay_io_failures: 0,
                ack_io_failures: 0,
                requests: Vec::new(),
                acks: Vec::new(),
            }));
            let mut transport = FakeReplayTransport {
                state: Arc::clone(&state),
            };
            assert!(matches!(
                consume_one_guardian_replay_snapshot(
                    &mut transport,
                    identity(),
                    size(24, 80),
                    test_terminal_config(),
                    TerminalCheckpointLimits::default(),
                    None,
                ),
                Err(GuardianProxyError::LeaseIdentityMismatch)
            ));
            assert!(
                state.lock().acks.is_empty(),
                "foreign page must fail before acknowledgement"
            );
        }
        for (expected_size, pixel_mismatch) in [
            (size(25, 80), false),
            (
                PtySize {
                    pixel_width: 641,
                    ..size(24, 80)
                },
                true,
            ),
        ] {
            let (descriptor, checkpoint) = genesis_checkpoint_fixture();
            let state = Arc::new(Mutex::new(FakeReplayState {
                pages: checkpoint_and_complete_pages(descriptor, checkpoint),
                replay_io_failures: 0,
                ack_io_failures: 0,
                requests: Vec::new(),
                acks: Vec::new(),
            }));
            let mut transport = FakeReplayTransport { state };
            let error = consume_one_guardian_replay_snapshot(
                &mut transport,
                identity(),
                expected_size,
                test_terminal_config(),
                TerminalCheckpointLimits::default(),
                None,
            )
            .expect_err("mismatched genesis geometry cannot activate");
            if pixel_mismatch {
                let GuardianProxyError::RestoredModel(source) = error else {
                    panic!("pixel mismatch did not reach model validation: {error:?}");
                };
                assert_eq!(
                    source.to_string(),
                    "checkpoint pixel geometry does not match the claimed topology manifest"
                );
            } else {
                assert!(matches!(
                    error,
                    GuardianProxyError::ReplayInvariant(
                        "checkpoint geometry does not match the claimed topology manifest"
                    )
                ));
            }
        }
        let (descriptor, checkpoint) = genesis_checkpoint_fixture();
        let mut pages = checkpoint_and_complete_pages(descriptor, checkpoint);
        let original = pages.pop_back().unwrap();
        pages.push_back(
            GuardianReplayPageDelivery::new(
                identity().pane_id(),
                identity().generation(),
                original.header().snapshot_id(),
                original.header().snapshot_digest(),
                original.header().incoming_cursor_digest(),
                1,
                None,
                GuardianReplayPageBodyDelivery::Complete {
                    checkpoint_id: descriptor.checkpoint_id(),
                    through_sequence: 1,
                    terminal_record_digest: [0x91; 32],
                    cumulative_plaintext_bytes: 1,
                },
            )
            .unwrap(),
        );
        let state = Arc::new(Mutex::new(FakeReplayState {
            pages,
            replay_io_failures: 0,
            ack_io_failures: 0,
            requests: Vec::new(),
            acks: Vec::new(),
        }));
        let mut transport = FakeReplayTransport {
            state: Arc::clone(&state),
        };
        assert!(matches!(
            consume_one_guardian_replay_snapshot(
                &mut transport,
                identity(),
                size(24, 80),
                test_terminal_config(),
                TerminalCheckpointLimits::default(),
                None,
            ),
            Err(GuardianProxyError::ReplayInvariant(
                "terminal replay witness does not match the consumed checkpoint and suffix"
            ))
        ));
        assert_eq!(
            state.lock().acks.len(),
            1,
            "forged terminal origin is never acknowledged"
        );
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

        let mut activated = staging
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
        let delivery = reader
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
        assert!(
            delivery.has_durable_replay_page_ack(),
            "page-terminal delivery reports its durable replay Ack"
        );
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
            identity().pane_id(),
            fixture.descriptor,
            &checkpoint,
            mismatched_size,
            test_terminal_config(),
            limits,
        )
        .expect_err("pixel geometry drift cannot become a live terminal");
        let GuardianProxyError::RestoredModel(source) = error else {
            panic!("pixel mismatch did not reach model validation: {error:?}");
        };
        assert_eq!(
            source.to_string(),
            "checkpoint pixel geometry does not match the claimed topology manifest"
        );
    }

    #[derive(Clone, Copy)]
    enum ClaimReplyFault {
        ExhaustThenRecover,
        RejectFirst,
        ReplaceAfterLostReply,
        WrongClaimReply,
    }

    fn exercise_authenticated_claim_fault(fault: ClaimReplyFault) {
        use mux::guardian_protocol::{
            GUARDIAN_MAX_FRAME_BYTES, GuardianEffectOutcome, GuardianOperation,
            GuardianProtocolState, GuardianRequestEnvelope, GuardianRequestHeader,
            GuardianResponseEnvelope, GuardianSecret, GuardianSpawnPayload,
            decode_guardian_request, encode_guardian_request, encode_guardian_response,
        };
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt as _;
        #[cfg(unix)]
        use std::os::unix::net::{UnixListener, UnixStream};

        // Protocol/transport fault injection only: the canonical ledger owns
        // the Claim, but this fixture does not create or claim to preserve a PTY.
        fn receive(stream: &mut UnixStream) -> Vec<u8> {
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).expect("frame prefix");
            let length = u32::from_be_bytes(prefix) as usize;
            assert!(length + 4 <= GUARDIAN_MAX_FRAME_BYTES);
            let mut frame = vec![0; length + 4];
            frame[..4].copy_from_slice(&prefix);
            stream.read_exact(&mut frame[4..]).expect("frame body");
            frame
        }
        let directory = tempfile::Builder::new()
            .prefix("ft-claim-retry-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(std::fs::canonicalize("/tmp").expect("canonical temporary root"))
            .expect("private fixture directory")
            .keep();
        let socket = directory.join("guardian.sock");
        let token = directory.join("token");
        frankenterm_pty_guardian::provision_guardian_token(&token).expect("private token");
        let token_bytes: [u8; 32] = std::fs::read(&token)
            .expect("fixture token")
            .try_into()
            .expect("fixed token");
        let listener = UnixListener::bind(&socket).expect("fixture listener");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("private socket mode");
        listener.set_nonblocking(true).expect("bounded accept");
        let lease_identity = identity();
        let request_id = Uuid::new_v4();
        let effect_id = Uuid::new_v4();
        let observed_generation = 0;
        let server = thread::spawn(move || {
            let secret = GuardianSecret::from_bytes(token_bytes).expect("fixture secret");
            let mut protocol = GuardianProtocolState::new(lease_identity.guardian_incarnation())
                .expect("canonical protocol ledger");
            let payload = GuardianSpawnPayload::new(
                portable_pty::CommandBuilder::new("/bin/sh"),
                size(24, 80),
            )
            .expect("fixture spawn shape")
            .encode()
            .expect("encode fixture spawn");
            let request = GuardianRequestEnvelope::from_zeroizing_payload(
                GuardianRequestHeader::new(
                    GuardianOperation::Spawn,
                    lease_identity.guardian_incarnation(),
                    lease_identity.mux_incarnation(),
                    Uuid::new_v4(),
                    Some(lease_identity.pane_id()),
                    0,
                    0,
                    Some(Uuid::new_v4()),
                    &payload,
                ),
                payload,
            );
            let frame = encode_guardian_request(&secret, &request).expect("encode fixture request");
            let request = decode_guardian_request(&secret, &frame).expect("authenticate fixture");
            protocol
                .apply_effect_transactionally(&request, |_| GuardianEffectOutcome::<()>::Applied)
                .expect("install protocol-only unclaimed pane");
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut applied_claims = 0;
            let attempts = match fault {
                ClaimReplyFault::ExhaustThenRecover => 33,
                ClaimReplyFault::RejectFirst | ClaimReplyFault::WrongClaimReply => 1,
                ClaimReplyFault::ReplaceAfterLostReply => 2,
            };
            for attempt in 0..attempts {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "bounded fixture accept");
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("read bound");
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("write bound");
                let hello = decode_guardian_request(&secret, &receive(&mut stream)).expect("Hello");
                let replaced =
                    matches!(fault, ClaimReplyFault::ReplaceAfterLostReply) && attempt == 1;
                let reply = if replaced {
                    GuardianReply::Hello {
                        guardian_incarnation: Uuid::new_v4(),
                    }
                } else {
                    protocol.apply_observation(&hello).expect("canonical Hello")
                };
                let response =
                    GuardianResponseEnvelope::reply(&hello, &reply).expect("Hello reply");
                stream
                    .write_all(&encode_guardian_response(&secret, &response).expect("Hello frame"))
                    .expect("send Hello");
                if replaced {
                    break;
                }
                let claim = decode_guardian_request(&secret, &receive(&mut stream)).expect("Claim");
                assert_eq!(claim.header().operation, GuardianOperation::Claim);
                assert_eq!(claim.header().request_id, request_id);
                assert_eq!(claim.header().effect_id, Some(effect_id));
                assert_eq!(claim.header().lease_generation, observed_generation);
                if matches!(fault, ClaimReplyFault::RejectFirst) {
                    let response = GuardianResponseEnvelope::rejection(
                        &claim,
                        GuardianRejectionCode::InvalidRequest,
                    );
                    stream
                        .write_all(
                            &encode_guardian_response(&secret, &response).expect("rejection frame"),
                        )
                        .expect("send authenticated rejection");
                    break;
                }
                let reply = protocol
                    .apply_effect_transactionally(&claim, |_| {
                        applied_claims += 1;
                        GuardianEffectOutcome::<()>::Applied
                    })
                    .expect("deduplicate exact Claim");
                if matches!(fault, ClaimReplyFault::WrongClaimReply) {
                    let other = GuardianRequestEnvelope::new(
                        GuardianRequestHeader::new(
                            GuardianOperation::Claim,
                            lease_identity.guardian_incarnation(),
                            lease_identity.mux_incarnation(),
                            Uuid::new_v4(),
                            Some(lease_identity.pane_id()),
                            observed_generation,
                            0,
                            Some(effect_id),
                            &[],
                        ),
                        Vec::new(),
                    );
                    let other = decode_guardian_request(
                        &secret,
                        &encode_guardian_request(&secret, &other).expect("other correlation"),
                    )
                    .expect("authenticated other request");
                    let response = GuardianResponseEnvelope::reply(&other, &reply)
                        .expect("authenticated incorrectly correlated reply");
                    stream
                        .write_all(
                            &encode_guardian_response(&secret, &response)
                                .expect("wrong reply frame"),
                        )
                        .expect("send wrong reply after actual Claim");
                    break;
                }
                if attempt < 32 {
                    // The real ledger applied Claim, but no response reaches the client.
                    continue;
                }
                let response =
                    GuardianResponseEnvelope::reply(&claim, &reply).expect("Claim reply");
                stream
                    .write_all(&encode_guardian_response(&secret, &response).expect("Claim frame"))
                    .expect("send Claim");
                let retire =
                    decode_guardian_request(&secret, &receive(&mut stream)).expect("Retire");
                assert_eq!(retire.header().operation, GuardianOperation::RetireLease);
                assert_eq!(retire.header().lease_generation, 1);
                let reply = protocol
                    .apply_effect_transactionally(&retire, |_| GuardianEffectOutcome::<()>::Applied)
                    .expect("canonical retirement");
                let response =
                    GuardianResponseEnvelope::reply(&retire, &reply).expect("Retire reply");
                stream
                    .write_all(&encode_guardian_response(&secret, &response).expect("Retire frame"))
                    .expect("send Retire");
            }
            assert_eq!(
                applied_claims,
                usize::from(!matches!(fault, ClaimReplyFault::RejectFirst)),
                "lost replies never repeat Claim effect"
            );
        });
        let (staging, _) = fake_staging([FakeDirective::Auto], 1);
        let census = Arc::clone(&staging.census);
        drop(staging);
        let plan =
            GuardianProxyLeasePlan::prepare(&socket, &token, size(24, 80), Arc::clone(&census))
                .expect("authenticated plan");
        let outcome = plan.claim(
            lease_identity.pane_id(),
            observed_generation,
            request_id,
            effect_id,
        );
        if matches!(fault, ClaimReplyFault::RejectFirst) {
            assert!(outcome.is_err());
            assert_eq!(
                census.retained_lease_cleanup_count(),
                0,
                "definitive first rejection releases reservation"
            );
            server.join().expect("join rejecting fixture");
            return;
        }
        match fault {
            ClaimReplyFault::ExhaustThenRecover => assert!(matches!(
                outcome,
                Err(GuardianProxyError::Client(GuardianClientError::Io(_)))
            )),
            ClaimReplyFault::ReplaceAfterLostReply => assert!(matches!(
                outcome,
                Err(GuardianProxyError::GuardianIncarnationChanged)
            )),
            ClaimReplyFault::RejectFirst => unreachable!(),
            ClaimReplyFault::WrongClaimReply => assert!(matches!(
                outcome,
                Err(GuardianProxyError::Client(GuardianClientError::Protocol(_)))
            )),
        }
        assert_eq!(census.retained_lease_cleanup_count(), 1);
        assert!(
            matches!(census.state.lock().retirement_slots[&lease_identity.pane_id()].authority.lock().as_ref(),
            Some(GuardianCleanupAuthority::PendingClaim(pending)) if pending.request_id == request_id && pending.effect_id == effect_id)
        );
        if matches!(
            fault,
            ClaimReplyFault::ReplaceAfterLostReply | ClaimReplyFault::WrongClaimReply
        ) {
            assert_eq!(census.blocked_retained_lease_cleanup_count(), 1);
            assert!(
                !census
                    .retry_retained_lease_cleanup()
                    .expect("blocked foreign authority cannot issue cleanup")
            );
            assert_eq!(census.retained_lease_cleanup_count(), 1);
        } else {
            assert!(
                census
                    .retry_retained_lease_cleanup()
                    .expect("recover genuine Claim then retire")
            );
            assert_eq!(census.retained_lease_cleanup_count(), 0);
        }
        server.join().expect("join canonical protocol fixture");
    }

    #[test]
    fn ambiguous_claim_retains_exact_request_until_authenticated_cleanup() {
        exercise_authenticated_claim_fault(ClaimReplyFault::ExhaustThenRecover);
    }

    #[test]
    fn definitive_claim_rejection_releases_reservation() {
        exercise_authenticated_claim_fault(ClaimReplyFault::RejectFirst);
    }

    #[test]
    fn ambiguous_claim_then_foreign_guardian_retains_blocked_request() {
        exercise_authenticated_claim_fault(ClaimReplyFault::ReplaceAfterLostReply);
    }

    #[test]
    fn applied_claim_with_uncorrelated_reply_retains_blocked_request() {
        exercise_authenticated_claim_fault(ClaimReplyFault::WrongClaimReply);
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
    fn unpublished_staging_drop_retains_exact_retirement_after_bounded_retries() {
        let (staging, state) = fake_staging(
            [FakeDirective::Io, FakeDirective::Io, FakeDirective::Auto],
            82,
        );
        let census = Arc::clone(&staging.census);
        assert_eq!(census.retained_lease_cleanup_count(), 1);

        drop(staging);

        assert_eq!(
            census.retained_lease_cleanup_count(),
            1,
            "lost synchronous retries retain the pre-reserved cleanup slot"
        );
        assert!(
            census
                .retry_retained_lease_cleanup()
                .expect("later cleanup maintenance confirms Retire"),
            "one bounded maintenance call retires one retained lease"
        );
        assert_eq!(census.retained_lease_cleanup_count(), 0);

        let retire_calls = state
            .lock()
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
        assert_eq!(retire_calls.len(), 3);
        assert!(retire_calls.iter().all(|call| *call == retire_calls[0]));
        assert_eq!(retire_calls[0].0, 82);
    }

    #[test]
    fn unpublished_retirement_absence_releases_reserved_cleanup_authority() {
        let (staging, state) = fake_staging(
            [FakeDirective::Reject(GuardianRejectionCode::PaneNotFound)],
            83,
        );
        let census = Arc::clone(&staging.census);

        drop(staging);

        assert_eq!(census.retained_lease_cleanup_count(), 0);
        assert_eq!(census.blocked_retained_lease_cleanup_count(), 0);
        let retire_calls = state
            .lock()
            .calls
            .iter()
            .filter(|call| matches!(call, FakeCall::Retire { .. }))
            .count();
        assert_eq!(retire_calls, 1, "known pane absence needs no retry");
    }

    #[test]
    fn unpublished_retirement_protocol_fault_is_retained_without_busy_retry() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Reject(GuardianRejectionCode::InternalInvariant),
                FakeDirective::Auto,
            ],
            84,
        );
        let census = Arc::clone(&staging.census);

        drop(staging);

        assert_eq!(census.retained_lease_cleanup_count(), 1);
        assert_eq!(census.blocked_retained_lease_cleanup_count(), 1);
        assert!(
            !census
                .retry_retained_lease_cleanup()
                .expect("blocked cleanup remains retained without another mutation")
        );
        let retire_calls = state
            .lock()
            .calls
            .iter()
            .filter(|call| matches!(call, FakeCall::Retire { .. }))
            .count();
        assert_eq!(retire_calls, 1, "blocked authority must not busy-retry");
    }

    #[test]
    fn retirement_retry_backoff_reaches_its_bounded_ceiling() {
        assert_eq!(
            guardian_retirement_retry_delay(1),
            GUARDIAN_RETIREMENT_RETRY_MIN_INTERVAL
        );
        assert_eq!(
            guardian_retirement_retry_delay(2),
            GUARDIAN_RETIREMENT_RETRY_MIN_INTERVAL.saturating_mul(2)
        );
        assert_eq!(
            guardian_retirement_retry_delay(8),
            GUARDIAN_RETIREMENT_RETRY_MAX_INTERVAL
        );
        assert_eq!(
            guardian_retirement_retry_delay(u32::MAX),
            GUARDIAN_RETIREMENT_RETRY_MAX_INTERVAL
        );
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
    fn tail_reader_reports_durable_page_ack_only_after_terminal_record() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor)
            .expect("derive multi-record tail boundary");
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_output_page_records(&fixture, &[b"first", b"second"])]),
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
        .expect("construct multi-record tail reader");

        let mut first_payload = None;
        let first = reader
            .deliver_next_record(&mut |_, _, payload| {
                first_payload = Some(payload);
                Ok(())
            })
            .expect("deliver first record inside the open replay page");
        assert_eq!(first_payload.as_deref(), Some(b"first".as_slice()));
        assert!(!first.has_durable_replay_page_ack());
        assert!(
            replay_state.lock().acks.is_empty(),
            "an interior record cannot report or emit the page Ack"
        );

        let mut second_payload = None;
        let second = reader
            .deliver_next_record(&mut |_, _, payload| {
                second_payload = Some(payload);
                Ok(())
            })
            .expect("deliver page-terminal record and durable Ack");
        assert_eq!(second_payload.as_deref(), Some(b"second".as_slice()));
        assert!(second.has_durable_replay_page_ack());
        assert_eq!(replay_state.lock().acks.len(), 1);
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
        assert!(reader.has_pending_transport_retry());
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
        assert!(!reader.has_pending_transport_retry());
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
        assert!(reader.has_pending_transport_retry());
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
        assert!(!reader.has_pending_transport_retry());
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
        assert!(reader.has_pending_transport_retry());
        let mut delivered = None;
        reader
            .deliver_next_record(&mut |_, _, payload| {
                delivered = Some(payload);
                Ok(())
            })
            .expect("retry the retained Replay");
        assert_eq!(delivered.as_deref(), Some(b"tail".as_slice()));
        assert!(!reader.has_pending_transport_retry());

        let state = replay_state.lock();
        assert_eq!(state.requests.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS + 1);
        assert!(
            state
                .requests
                .iter()
                .all(|request| *request == state.requests[0])
        );
        assert_eq!(
            state.acks.len(),
            1,
            "successful record delivery is acknowledged"
        );
    }

    #[test]
    fn tail_transport_retry_after_delivery_never_redelivers_parser_bytes() {
        let fixture = capture_record_checkpoint_fixture();
        let boundary = GuardianReplayBoundary::from_descriptor(fixture.descriptor).unwrap();
        let replay_state = Arc::new(Mutex::new(FakeReplayState {
            pages: VecDeque::from([tail_output_page(&fixture, b"once")]),
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
        .unwrap();
        let mut deliveries = 0;
        let first_error = reader
            .deliver_next_record(&mut |_, _, payload| {
                assert_eq!(payload.as_ref(), b"once");
                deliveries += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(first_error.kind(), io::ErrorKind::ConnectionReset);
        assert!(reader.has_pending_transport_retry());
        assert_eq!(deliveries, 1);
        let second_error = reader
            .deliver_next_record(&mut |_, _, _| {
                deliveries += 1;
                Ok(())
            })
            .expect_err("fake source ends after the exact retained Ack succeeds");
        assert_eq!(second_error.kind(), io::ErrorKind::InvalidData);
        assert!(!reader.has_pending_transport_retry());
        assert_eq!(deliveries, 1, "retry must never reapply acknowledged bytes");
        let state = replay_state.lock();
        assert_eq!(state.acks.len(), GUARDIAN_REPLAY_EXCHANGE_ATTEMPTS + 1);
        assert!(state.acks.iter().all(|ack| *ack == state.acks[0]));
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
        assert!(!reader.has_pending_transport_retry());
        assert_eq!(
            replay_state.lock().acks,
            Vec::new(),
            "failed parser delivery cannot acknowledge its replay page"
        );

        let terminal_error = reader
            .deliver_next_record(&mut |_, _, _| Ok(()))
            .expect_err("failed delivery makes this reader terminal");
        assert_eq!(terminal_error.kind(), io::ErrorKind::InvalidData);
        assert!(!reader.has_pending_transport_retry());
        let state = replay_state.lock();
        assert_eq!(state.requests.len(), 1);
        assert_eq!(state.acks, Vec::new());
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
    fn child_wait_retries_busy_census_without_closing_live_child() {
        let (staging, state) = fake_staging(
            [
                FakeDirective::Io,
                FakeDirective::Observe(ObservedChildState::Exited(23)),
            ],
            61,
        );
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        assert_eq!(child.wait().unwrap().exit_code(), 23);
        assert_eq!(actor.lock().next_sequence(), 61);
        assert_eq!(state.lock().calls, vec![FakeCall::Census, FakeCall::Census]);
    }

    #[test]
    fn child_wait_does_not_retry_a_fenced_lease() {
        let (staging, state) = fake_staging(
            [FakeDirective::Reject(GuardianRejectionCode::StaleLease)],
            61,
        );
        let actor = staging.shared_actor();
        let mut child = GuardianProxyChild {
            actor: Arc::clone(&actor),
            census: Arc::clone(&staging.census),
        };
        assert_eq!(child.wait().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(actor.lock().disposition, GuardianLeaseDisposition::Fenced);
        assert_eq!(actor.lock().next_sequence(), 61);
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
    fn terminal_observation_permits_retire_and_close_after_full_input_recovery() {
        for mutation in ["retire", "close"] {
            let (staging, state) = fake_staging(
                [
                    FakeDirective::Io,
                    FakeDirective::Observe(ObservedChildState::Exited(24)),
                    FakeDirective::Query(InputEffectState::DurableFull),
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
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::TerminalObserved
            );
            if mutation == "retire" {
                assert!(actor.lock().retire(identity()).is_ok());
            } else {
                assert!(actor.lock().close(identity()).is_ok());
            }
            assert_eq!(actor.lock().disposition, GuardianLeaseDisposition::Closed);
            assert_eq!(actor.lock().next_sequence(), 65);
            assert!(matches!(
                actor.lock().resize(size(25, 80)),
                Err(GuardianProxyError::LeaseNotAttached)
            ));
            assert!(actor.lock().close(identity()).is_ok());
            assert!(actor.lock().retire(identity()).is_ok());
            assert!(matches!(
                state.lock().calls.as_slice(),
                [
                    FakeCall::Input { .. },
                    FakeCall::Census,
                    FakeCall::QueryInput { .. },
                ]
            ));
        }
    }

    #[test]
    fn terminal_observation_rejects_retire_and_close_on_partial_input_recovery() {
        for mutation in ["retire", "close"] {
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
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::TerminalObserved
            );
            let outcome = if mutation == "retire" {
                actor.lock().retire(identity())
            } else {
                actor.lock().close(identity())
            };
            assert!(matches!(
                outcome,
                Err(GuardianProxyError::PreviousInputPartiallyApplied {
                    applied_bytes: 3,
                    input_bytes: 6,
                })
            ));
            assert_eq!(actor.lock().disposition, GuardianLeaseDisposition::Closed);
            assert_eq!(actor.lock().next_sequence(), 65);
            if mutation == "retire" {
                assert!(actor.lock().retire(identity()).is_ok());
            } else {
                assert!(actor.lock().close(identity()).is_ok());
            }
            assert!(matches!(
                state.lock().calls.as_slice(),
                [
                    FakeCall::Input { .. },
                    FakeCall::Census,
                    FakeCall::QueryInput { .. },
                ]
            ));
        }
    }

    #[test]
    fn terminal_observation_rejects_retire_and_close_on_unknown_input_recovery() {
        for mutation in ["retire", "close"] {
            let (staging, _state) = fake_staging(
                [
                    FakeDirective::Io,
                    FakeDirective::Observe(ObservedChildState::Exited(24)),
                    FakeDirective::Query(InputEffectState::DispositionUnavailable),
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
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::TerminalObserved
            );
            let outcome = if mutation == "retire" {
                actor.lock().retire(identity())
            } else {
                actor.lock().close(identity())
            };
            assert!(matches!(
                outcome,
                Err(GuardianProxyError::InputDispositionUnavailable)
            ));
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::Quarantined
            );
            assert!(matches!(
                actor.lock().close(identity()),
                Err(GuardianProxyError::PaneQuarantined)
            ));
            assert!(matches!(
                actor.lock().retire(identity()),
                Err(GuardianProxyError::PaneQuarantined)
            ));

            let (staging, _state) = fake_staging(
                [
                    FakeDirective::Io,
                    FakeDirective::Observe(ObservedChildState::Exited(24)),
                    FakeDirective::Query(InputEffectState::AcceptedNotDurable),
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
            assert_eq!(child.try_wait().unwrap().unwrap().exit_code(), 24);
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::TerminalObserved
            );
            let outcome = if mutation == "retire" {
                actor.lock().retire(identity())
            } else {
                actor.lock().close(identity())
            };
            assert!(matches!(
                outcome,
                Err(GuardianProxyError::InputDurabilityPending)
            ));
            assert_eq!(
                actor.lock().disposition,
                GuardianLeaseDisposition::TerminalObserved
            );
        }
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
