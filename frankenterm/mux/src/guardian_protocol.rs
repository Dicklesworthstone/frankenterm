//! Authenticated, bounded protocol and pure fencing state machine for the PTY guardian.
//!
//! This module deliberately contains no sockets, PTYs, subprocesses, or mux-global lookups.
//! A transport must decode and authenticate a complete frame here before it is allowed to
//! route the request to a pane runtime.  The pure state machine is the authority for spawn
//! idempotency, lease generations, mutation sequencing, and ambiguous input reconciliation.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const GUARDIAN_PROTOCOL_VERSION: u16 = 1;
pub const GUARDIAN_AUTH_TOKEN_BYTES: usize = 32;
pub const GUARDIAN_MAC_BYTES: usize = 32;
pub const GUARDIAN_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const GUARDIAN_MAX_PAYLOAD_BYTES: usize = 512 * 1024;
pub const GUARDIAN_MAX_INPUT_BYTES: usize = 64 * 1024;
pub const GUARDIAN_MAX_PANES: usize = 16_384;
pub const GUARDIAN_MAX_EFFECT_RECEIPTS: usize = 65_536;
pub const GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT: usize = 64;
pub const GUARDIAN_MAX_CENSUS_ENTRIES: u16 = 256;
pub const GUARDIAN_MAX_CENSUS_BYTES: u32 = 256 * 1024;
pub const GUARDIAN_MAX_CENSUS_SNAPSHOTS: usize = 8;
pub const GUARDIAN_CENSUS_PAGE_HEADER_BYTES: u32 = 34;
pub const GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES: u32 = 71;
pub const GUARDIAN_MIN_CENSUS_PAGE_BYTES: u32 =
    GUARDIAN_CENSUS_PAGE_HEADER_BYTES + GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES;

const FRAME_MAGIC: [u8; 4] = *b"FTG1";
const RESPONSE_FRAME_MAGIC: [u8; 4] = *b"FTR1";
const FRAME_LENGTH_BYTES: usize = 4;
const REQUEST_FRAME_HEADER_BYTES: usize = 140;
const REQUEST_FRAME_MIN_BYTES: usize =
    FRAME_LENGTH_BYTES + REQUEST_FRAME_HEADER_BYTES + GUARDIAN_MAC_BYTES;
const REQUEST_PAYLOAD_LENGTH_OFFSET: usize =
    FRAME_LENGTH_BYTES + REQUEST_FRAME_HEADER_BYTES - std::mem::size_of::<u32>();
const RESPONSE_FRAME_HEADER_BYTES: usize = 172;
const RESPONSE_FRAME_MIN_BYTES: usize =
    FRAME_LENGTH_BYTES + RESPONSE_FRAME_HEADER_BYTES + GUARDIAN_MAC_BYTES;
const RESPONSE_PAYLOAD_LENGTH_OFFSET: usize =
    FRAME_LENGTH_BYTES + RESPONSE_FRAME_HEADER_BYTES - std::mem::size_of::<u32>();

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct GuardianSecret([u8; GUARDIAN_AUTH_TOKEN_BYTES]);

impl GuardianSecret {
    pub fn from_bytes(
        bytes: [u8; GUARDIAN_AUTH_TOKEN_BYTES],
    ) -> Result<Self, GuardianProtocolError> {
        let combined = bytes.iter().fold(0_u8, |accumulator, byte| accumulator | byte);
        if combined == 0 {
            return Err(GuardianProtocolError::WeakSecret);
        }
        Ok(Self(bytes))
    }

    fn mac(
        &self,
        authenticated_bytes: &[u8],
    ) -> Result<[u8; GUARDIAN_MAC_BYTES], GuardianProtocolError> {
        let mut mac = HmacSha256::new_from_slice(&self.0)
            .map_err(|_| GuardianProtocolError::SecretInitializationFailed)?;
        mac.update(authenticated_bytes);
        let output = mac.finalize().into_bytes();
        let mut tag = [0_u8; GUARDIAN_MAC_BYTES];
        tag.copy_from_slice(&output);
        Ok(tag)
    }

    fn verify(&self, authenticated_bytes: &[u8], tag: &[u8]) -> Result<(), GuardianProtocolError> {
        let mut mac = HmacSha256::new_from_slice(&self.0)
            .map_err(|_| GuardianProtocolError::SecretInitializationFailed)?;
        mac.update(authenticated_bytes);
        mac.verify_slice(tag)
            .map_err(|_| GuardianProtocolError::AuthenticationFailed)
    }
}

impl std::fmt::Debug for GuardianSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GuardianSecret([REDACTED])")
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GuardianOperation {
    Spawn = 1,
    Census = 2,
    Claim = 3,
    Attach = 4,
    Input = 5,
    Resize = 6,
    Signal = 7,
    Close = 8,
    Checkpoint = 9,
    Replay = 10,
    QueryInputEffect = 11,
    RetireLease = 12,
}

impl GuardianOperation {
    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            1 => Ok(Self::Spawn),
            2 => Ok(Self::Census),
            3 => Ok(Self::Claim),
            4 => Ok(Self::Attach),
            5 => Ok(Self::Input),
            6 => Ok(Self::Resize),
            7 => Ok(Self::Signal),
            8 => Ok(Self::Close),
            9 => Ok(Self::Checkpoint),
            10 => Ok(Self::Replay),
            11 => Ok(Self::QueryInputEffect),
            12 => Ok(Self::RetireLease),
            other => Err(GuardianProtocolError::UnknownOperation(other)),
        }
    }

    #[must_use]
    pub const fn creates_effect(self) -> bool {
        matches!(
            self,
            Self::Spawn
                | Self::Claim
                | Self::Input
                | Self::Resize
                | Self::Signal
                | Self::Close
                | Self::Checkpoint
                | Self::RetireLease
        )
    }

    const fn requires_lease(self) -> bool {
        matches!(
            self,
            Self::Attach
                | Self::Input
                | Self::Resize
                | Self::Signal
                | Self::Close
                | Self::Checkpoint
                | Self::Replay
                | Self::QueryInputEffect
                | Self::RetireLease
        )
    }

    const fn uses_mutation_sequence(self) -> bool {
        matches!(
            self,
            Self::Input
                | Self::Resize
                | Self::Signal
                | Self::Close
                | Self::Checkpoint
                | Self::RetireLease
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianRequestHeader {
    pub protocol_version: u16,
    pub operation: GuardianOperation,
    pub guardian_incarnation: Uuid,
    pub mux_incarnation: Uuid,
    pub request_id: Uuid,
    pub payload_sha256: [u8; 32],
    pub pane_id: Option<Uuid>,
    pub lease_generation: u64,
    pub lease_sequence: u64,
    pub effect_id: Option<Uuid>,
}

impl GuardianRequestHeader {
    #[must_use]
    pub fn new(
        operation: GuardianOperation,
        guardian_incarnation: Uuid,
        mux_incarnation: Uuid,
        request_id: Uuid,
        pane_id: Option<Uuid>,
        lease_generation: u64,
        lease_sequence: u64,
        effect_id: Option<Uuid>,
        payload: &[u8],
    ) -> Self {
        Self {
            protocol_version: GUARDIAN_PROTOCOL_VERSION,
            operation,
            guardian_incarnation,
            mux_incarnation,
            request_id,
            payload_sha256: Sha256::digest(payload).into(),
            pane_id,
            lease_generation,
            lease_sequence,
            effect_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GuardianRequestEnvelope {
    pub header: GuardianRequestHeader,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for GuardianRequestEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianRequestEnvelope")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl GuardianRequestEnvelope {
    #[must_use]
    pub fn new(header: GuardianRequestHeader, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticatedGuardianRequest(GuardianRequestEnvelope);

impl std::fmt::Debug for AuthenticatedGuardianRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AuthenticatedGuardianRequest")
            .field(&self.0)
            .finish()
    }
}

impl std::ops::Deref for AuthenticatedGuardianRequest {
    type Target = GuardianRequestEnvelope;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianResponseStatus {
    Success = 0,
    Rejected = 1,
    Terminal = 2,
}

impl GuardianResponseStatus {
    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            0 => Ok(Self::Success),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Terminal),
            other => Err(GuardianProtocolError::UnknownResponseStatus(other)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianResponseHeader {
    pub protocol_version: u16,
    pub operation: GuardianOperation,
    pub status: GuardianResponseStatus,
    pub guardian_incarnation: Uuid,
    pub mux_incarnation: Uuid,
    pub request_id: Uuid,
    pub request_payload_sha256: [u8; 32],
    pub payload_sha256: [u8; 32],
    pub pane_id: Option<Uuid>,
    pub lease_generation: u64,
    pub lease_sequence: u64,
    pub effect_id: Option<Uuid>,
}

impl GuardianResponseHeader {
    #[must_use]
    pub fn new(
        request: &GuardianRequestHeader,
        status: GuardianResponseStatus,
        payload: &[u8],
    ) -> Self {
        Self {
            protocol_version: GUARDIAN_PROTOCOL_VERSION,
            operation: request.operation,
            status,
            guardian_incarnation: request.guardian_incarnation,
            mux_incarnation: request.mux_incarnation,
            request_id: request.request_id,
            request_payload_sha256: request.payload_sha256,
            payload_sha256: Sha256::digest(payload).into(),
            pane_id: request.pane_id,
            lease_generation: request.lease_generation,
            lease_sequence: request.lease_sequence,
            effect_id: request.effect_id,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GuardianResponseEnvelope {
    pub header: GuardianResponseHeader,
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for GuardianResponseEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianResponseEnvelope")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticatedGuardianResponse(GuardianResponseEnvelope);

impl std::fmt::Debug for AuthenticatedGuardianResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AuthenticatedGuardianResponse")
            .field(&self.0)
            .finish()
    }
}

impl AuthenticatedGuardianResponse {
    pub fn correlate(
        self,
        request: &GuardianRequestHeader,
    ) -> Result<CorrelatedGuardianResponse, GuardianProtocolError> {
        let response = &self.0.header;
        if response.protocol_version != request.protocol_version
            || response.operation != request.operation
            || response.guardian_incarnation != request.guardian_incarnation
            || response.mux_incarnation != request.mux_incarnation
            || response.request_id != request.request_id
            || response.request_payload_sha256 != request.payload_sha256
            || response.pane_id != request.pane_id
            || response.lease_generation != request.lease_generation
            || response.lease_sequence != request.lease_sequence
            || response.effect_id != request.effect_id
        {
            return Err(GuardianProtocolError::ResponseRequestMismatch);
        }
        Ok(CorrelatedGuardianResponse(self.0))
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CorrelatedGuardianResponse(GuardianResponseEnvelope);

impl std::fmt::Debug for CorrelatedGuardianResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("CorrelatedGuardianResponse")
            .field(&self.0)
            .finish()
    }
}

impl CorrelatedGuardianResponse {
    #[must_use]
    pub const fn header(&self) -> &GuardianResponseHeader {
        &self.0.header
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.0.payload
    }

    #[must_use]
    pub const fn envelope(&self) -> &GuardianResponseEnvelope {
        &self.0
    }
}

impl AuthenticatedGuardianRequest {
    #[must_use]
    pub const fn header(&self) -> &GuardianRequestHeader {
        &self.0.header
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.0.payload
    }

    #[must_use]
    pub const fn envelope(&self) -> &GuardianRequestEnvelope {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianCensusPageRequest {
    pub snapshot_id: Uuid,
    pub cursor: u64,
    pub max_entries: u16,
    pub max_bytes: u32,
}

impl GuardianCensusPageRequest {
    pub const ENCODED_BYTES: usize = 30;

    pub fn new(
        snapshot_id: Uuid,
        cursor: u64,
        max_entries: u16,
        max_bytes: u32,
    ) -> Result<Self, GuardianProtocolError> {
        let request = Self {
            snapshot_id,
            cursor,
            max_entries,
            max_bytes,
        };
        request.validate()?;
        Ok(request)
    }

    #[must_use]
    pub fn encode(self) -> [u8; Self::ENCODED_BYTES] {
        let mut encoded = [0_u8; Self::ENCODED_BYTES];
        encoded[..16].copy_from_slice(self.snapshot_id.as_bytes());
        encoded[16..24].copy_from_slice(&self.cursor.to_be_bytes());
        encoded[24..26].copy_from_slice(&self.max_entries.to_be_bytes());
        encoded[26..30].copy_from_slice(&self.max_bytes.to_be_bytes());
        encoded
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != Self::ENCODED_BYTES {
            return Err(GuardianProtocolError::InvalidCensusPage);
        }
        let mut snapshot_id = [0_u8; 16];
        snapshot_id.copy_from_slice(&payload[..16]);
        let mut cursor = [0_u8; 8];
        cursor.copy_from_slice(&payload[16..24]);
        let mut max_entries = [0_u8; 2];
        max_entries.copy_from_slice(&payload[24..26]);
        let mut max_bytes = [0_u8; 4];
        max_bytes.copy_from_slice(&payload[26..30]);
        let request = Self {
            snapshot_id: Uuid::from_bytes(snapshot_id),
            cursor: u64::from_be_bytes(cursor),
            max_entries: u16::from_be_bytes(max_entries),
            max_bytes: u32::from_be_bytes(max_bytes),
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        if self.snapshot_id.is_nil()
            || self.max_entries == 0
            || self.max_entries > GUARDIAN_MAX_CENSUS_ENTRIES
            || self.max_bytes < GUARDIAN_MIN_CENSUS_PAGE_BYTES
            || self.max_bytes > GUARDIAN_MAX_CENSUS_BYTES
        {
            return Err(GuardianProtocolError::InvalidCensusPage);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEffectState {
    NotSeen,
    AcceptedNotDurable,
    DurableEffect,
    TerminalRejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCensusPaneStatus {
    LiveUnclaimed,
    LiveClaimed,
    ExitedUnclaimed,
    ClosedTerminal,
    Quarantined,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardianCensusEntry {
    pub pane_id: Uuid,
    pub status: GuardianCensusPaneStatus,
    pub generation: u64,
    pub mux_incarnation: Option<Uuid>,
    pub next_sequence: Option<u64>,
    pub pending_input_effect: Option<Uuid>,
    pub exit_status: Option<i32>,
    pub quarantine_reason: Option<GuardianQuarantineReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuardianReply {
    Spawned { pane_id: Uuid, generation: u64 },
    CensusPage {
        snapshot_id: Uuid,
        entries: Vec<GuardianCensusEntry>,
        next_cursor: Option<u64>,
        total_panes: u64,
    },
    Claimed { pane_id: Uuid, generation: u64, next_sequence: u64 },
    Attached { pane_id: Uuid, generation: u64, next_sequence: u64 },
    InputReceipt {
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
        state: InputEffectState,
    },
    MutationApplied { pane_id: Uuid, generation: u64, sequence: u64 },
    LeaseRetired { pane_id: Uuid, generation: u64 },
    InputEffect { effect_id: Uuid, state: InputEffectState },
    ReplayReady { pane_id: Uuid, generation: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuardianPaneState {
    LiveUnclaimed {
        generation: u64,
    },
    LiveClaimed {
        generation: u64,
        mux_incarnation: Uuid,
        next_sequence: u64,
        pending_input_effect: Option<Uuid>,
    },
    ExitedUnclaimed {
        generation: u64,
        exit_status: i32,
        pending_input_effect: Option<Uuid>,
    },
    ClosedTerminal {
        generation: u64,
        exit_status: Option<i32>,
    },
    Quarantined {
        generation: u64,
        reason: GuardianQuarantineReason,
        exit_status: Option<i32>,
    },
}

impl GuardianPaneState {
    #[must_use]
    pub const fn generation(&self) -> u64 {
        match self {
            Self::LiveUnclaimed { generation }
            | Self::LiveClaimed { generation, .. }
            | Self::ExitedUnclaimed { generation, .. }
            | Self::ClosedTerminal { generation, .. }
            | Self::Quarantined { generation, .. } => *generation,
        }
    }
}

impl GuardianCensusEntry {
    fn from_state(pane_id: Uuid, state: &GuardianPaneState) -> Self {
        match state {
            GuardianPaneState::LiveUnclaimed { generation } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::LiveUnclaimed,
                generation: *generation,
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                exit_status: None,
                quarantine_reason: None,
            },
            GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                pending_input_effect,
            } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::LiveClaimed,
                generation: *generation,
                mux_incarnation: Some(*mux_incarnation),
                next_sequence: Some(*next_sequence),
                pending_input_effect: *pending_input_effect,
                exit_status: None,
                quarantine_reason: None,
            },
            GuardianPaneState::ExitedUnclaimed {
                generation,
                exit_status,
                pending_input_effect,
            } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::ExitedUnclaimed,
                generation: *generation,
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: *pending_input_effect,
                exit_status: Some(*exit_status),
                quarantine_reason: None,
            },
            GuardianPaneState::ClosedTerminal {
                generation,
                exit_status,
            } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::ClosedTerminal,
                generation: *generation,
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                exit_status: *exit_status,
                quarantine_reason: None,
            },
            GuardianPaneState::Quarantined {
                generation,
                reason,
                exit_status,
            } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::Quarantined,
                generation: *generation,
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                exit_status: *exit_status,
                quarantine_reason: Some(*reason),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianQuarantineReason {
    GenerationExhausted,
    SequenceExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GuardianProtocolError {
    #[error("guardian frame is shorter than the authenticated v1 envelope")]
    TruncatedFrame,
    #[error("guardian frame length {actual} does not match declared length {declared}")]
    FrameLengthMismatch { declared: usize, actual: usize },
    #[error("guardian frame exceeds the {GUARDIAN_MAX_FRAME_BYTES}-byte ceiling")]
    FrameTooLarge,
    #[error("guardian payload exceeds the {GUARDIAN_MAX_PAYLOAD_BYTES}-byte ceiling")]
    PayloadTooLarge,
    #[error("guardian frame has an invalid magic value")]
    InvalidMagic,
    #[error("guardian protocol version {0} is unsupported")]
    UnsupportedVersion(u16),
    #[error("guardian operation discriminant {0} is unknown")]
    UnknownOperation(u8),
    #[error("guardian response status discriminant {0} is unknown")]
    UnknownResponseStatus(u8),
    #[error("guardian frame reserved flags are nonzero")]
    ReservedFlags,
    #[error("guardian frame authentication failed")]
    AuthenticationFailed,
    #[error("authenticated guardian response does not match the originating request identity")]
    ResponseRequestMismatch,
    #[error("guardian HMAC could not initialize from the frozen token length")]
    SecretInitializationFailed,
    #[error("guardian authentication token is the reserved all-zero value")]
    WeakSecret,
    #[error("guardian payload SHA-256 does not match the authenticated header")]
    PayloadDigestMismatch,
    #[error("guardian request has a reserved zero {0} identity")]
    ZeroIdentity(&'static str),
    #[error("guardian request operation {operation:?} has invalid scope fields")]
    InvalidOperationScope { operation: GuardianOperation },
    #[error("guardian request targets a retired guardian incarnation")]
    GuardianIncarnationMismatch,
    #[error("guardian pane {0} does not exist")]
    PaneNotFound(Uuid),
    #[error("guardian pane {0} already exists under a different spawn identity")]
    PaneAlreadyExists(Uuid),
    #[error("guardian request UUID was reused with different authenticated bytes")]
    RequestIdentityConflict,
    #[error("guardian effect UUID was reused with different authenticated bytes")]
    EffectIdentityConflict,
    #[error("guardian pane is terminal or quarantined")]
    PaneTerminal,
    #[error("guardian claim observed generation {observed}, current generation is {current}")]
    ClaimGenerationMismatch { observed: u64, current: u64 },
    #[error("guardian lease generation is stale or belongs to a different mux incarnation")]
    StaleLease,
    #[error("guardian lease sequence {observed} repeats retired sequence {expected}")]
    RepeatedSequence { expected: u64, observed: u64 },
    #[error("guardian lease sequence {observed} skips required sequence {expected}")]
    SequenceGap { expected: u64, observed: u64 },
    #[error("guardian lease generation space is exhausted; pane quarantined")]
    GenerationExhausted,
    #[error("guardian lease mutation sequence space is exhausted; pane quarantined")]
    SequenceExhausted,
    #[error("guardian pane or effect receipt bound is exhausted before mutation")]
    CapacityExhausted,
    #[error(
        "guardian pending effect {effect_id} reached its {max_aliases}-request alias ceiling"
    )]
    RequestAliasCapacityExhausted {
        effect_id: Uuid,
        max_aliases: usize,
    },
    #[error("guardian protocol state invariant failed at {0}")]
    StateInvariantViolation(&'static str),
    #[error("guardian input-effect query omitted the effect UUID")]
    MissingEffectQueryIdentity,
    #[error("guardian pane has an accepted input awaiting durable journal acknowledgement")]
    InputDurabilityPending,
    #[error("guardian input durability acknowledgement does not match the pending pane effect")]
    InputDurabilityIdentityMismatch,
    #[error("guardian census page has an invalid encoding or exceeds its entry/byte cap")]
    InvalidCensusPage,
    #[error("guardian census cursor {cursor} exceeds pane count {pane_count}")]
    InvalidCensusCursor { cursor: u64, pane_count: u64 },
    #[error("guardian census snapshot {0} is unavailable or has rotated")]
    CensusSnapshotNotFound(Uuid),
    #[error("guardian census snapshot UUID was reused by a different mux incarnation")]
    CensusSnapshotIdentityConflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct EffectFingerprint {
    operation: GuardianOperation,
    pane_id: Uuid,
    mux_incarnation: Uuid,
    lease_generation: u64,
    lease_sequence: u64,
    payload_sha256: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredEffect {
    fingerprint: EffectFingerprint,
    reply: GuardianReply,
    state: InputEffectState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredRequest {
    fingerprint: EffectFingerprint,
    effect_id: Uuid,
    reply: GuardianReply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GuardianCensusSnapshot {
    mux_incarnation: Uuid,
    entries: Vec<GuardianCensusEntry>,
}

#[derive(Debug)]
pub struct GuardianProtocolState {
    incarnation: Uuid,
    panes: BTreeMap<Uuid, GuardianPaneState>,
    census_snapshots: HashMap<Uuid, GuardianCensusSnapshot>,
    census_snapshot_order: VecDeque<Uuid>,
    requests: HashMap<Uuid, StoredRequest>,
    effects: HashMap<Uuid, StoredEffect>,
    effect_request_ids: HashMap<Uuid, HashSet<Uuid>>,
    // Original spawn identities are deliberately absent from these queues:
    // forgetting one could turn a delayed retry into a second child. Every
    // other effect is protected after eviction by its pane generation and
    // mutation sequence, so the finite replay window may rotate safely.
    transient_request_order: VecDeque<Uuid>,
    transient_effect_order: VecDeque<Uuid>,
    protected_spawn_requests: HashSet<Uuid>,
    protected_spawn_effects: HashSet<Uuid>,
    receipt_capacity: usize,
}

impl GuardianProtocolState {
    pub fn new(incarnation: Uuid) -> Result<Self, GuardianProtocolError> {
        require_nonzero(incarnation, "guardian incarnation")?;
        Ok(Self {
            incarnation,
            panes: BTreeMap::new(),
            census_snapshots: HashMap::new(),
            census_snapshot_order: VecDeque::new(),
            requests: HashMap::new(),
            effects: HashMap::new(),
            effect_request_ids: HashMap::new(),
            transient_request_order: VecDeque::new(),
            transient_effect_order: VecDeque::new(),
            protected_spawn_requests: HashSet::new(),
            protected_spawn_effects: HashSet::new(),
            receipt_capacity: GUARDIAN_MAX_EFFECT_RECEIPTS,
        })
    }

    #[cfg(test)]
    fn new_with_receipt_capacity(
        incarnation: Uuid,
        receipt_capacity: usize,
    ) -> Result<Self, GuardianProtocolError> {
        if receipt_capacity == 0 {
            return Err(GuardianProtocolError::CapacityExhausted);
        }
        let mut state = Self::new(incarnation)?;
        state.receipt_capacity = receipt_capacity;
        Ok(state)
    }

    #[must_use]
    pub const fn incarnation(&self) -> Uuid {
        self.incarnation
    }

    #[must_use]
    pub fn pane_state(&self, pane_id: Uuid) -> Option<&GuardianPaneState> {
        self.panes.get(&pane_id)
    }

    pub fn mark_exited(&mut self, pane_id: Uuid, exit_status: i32) -> Result<(), GuardianProtocolError> {
        let state = self
            .panes
            .get_mut(&pane_id)
            .ok_or(GuardianProtocolError::PaneNotFound(pane_id))?;
        let generation = state.generation();
        match state {
            GuardianPaneState::LiveUnclaimed { .. } => {
                *state = GuardianPaneState::ExitedUnclaimed {
                    generation,
                    exit_status,
                    pending_input_effect: None,
                };
                Ok(())
            }
            GuardianPaneState::LiveClaimed {
                pending_input_effect,
                ..
            } => {
                let pending_input_effect = *pending_input_effect;
                *state = GuardianPaneState::ExitedUnclaimed {
                    generation,
                    exit_status,
                    pending_input_effect,
                };
                Ok(())
            }
            GuardianPaneState::Quarantined { exit_status: slot, .. } if slot.is_none() => {
                *slot = Some(exit_status);
                Ok(())
            }
            GuardianPaneState::ClosedTerminal { exit_status: slot, .. } if slot.is_none() => {
                *slot = Some(exit_status);
                Ok(())
            }
            GuardianPaneState::ExitedUnclaimed { .. }
            | GuardianPaneState::ClosedTerminal { .. }
            | GuardianPaneState::Quarantined { .. } => Err(GuardianProtocolError::PaneTerminal),
        }
    }

    pub fn apply(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch);
        }

        match request.header.operation {
            GuardianOperation::Census => {
                let page = GuardianCensusPageRequest::decode(&request.payload)?;
                self.census(page, request.header.mux_incarnation)
            }
            GuardianOperation::Attach => self.attach(request),
            GuardianOperation::Replay => self.replay(request),
            GuardianOperation::QueryInputEffect => self.query_input_effect(request),
            operation if operation.creates_effect() => self.apply_effect(request),
            _ => Err(GuardianProtocolError::StateInvariantViolation(
                "operation-classification",
            )),
        }
    }

    pub fn mark_input_durable(
        &mut self,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(effect_id, InputEffectState::DurableEffect)
    }

    pub fn mark_input_terminal_rejected(
        &mut self,
        effect_id: Uuid,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(effect_id, InputEffectState::TerminalRejected)
    }

    fn census(
        &mut self,
        page: GuardianCensusPageRequest,
        mux_incarnation: Uuid,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if self
            .census_snapshots
            .get(&page.snapshot_id)
            .is_some_and(|snapshot| snapshot.mux_incarnation != mux_incarnation)
        {
            return Err(GuardianProtocolError::CensusSnapshotIdentityConflict);
        }
        if page.cursor == 0 && !self.census_snapshots.contains_key(&page.snapshot_id) {
            let entries = self
                .panes
                .iter()
                .map(|(pane_id, state)| GuardianCensusEntry::from_state(*pane_id, state))
                .collect::<Vec<_>>();
            while self.census_snapshots.len() >= GUARDIAN_MAX_CENSUS_SNAPSHOTS {
                let retired = self
                    .census_snapshot_order
                    .pop_front()
                    .ok_or(GuardianProtocolError::CapacityExhausted)?;
                self.census_snapshots.remove(&retired);
            }
            self.census_snapshots.insert(
                page.snapshot_id,
                GuardianCensusSnapshot {
                    mux_incarnation,
                    entries,
                },
            );
            self.census_snapshot_order.push_back(page.snapshot_id);
        }
        let snapshot = self
            .census_snapshots
            .get(&page.snapshot_id)
            .ok_or(GuardianProtocolError::CensusSnapshotNotFound(
                page.snapshot_id,
            ))?;
        let total_panes = u64::try_from(snapshot.entries.len())
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        if page.cursor > total_panes {
            return Err(GuardianProtocolError::InvalidCensusCursor {
                cursor: page.cursor,
                pane_count: total_panes,
            });
        }
        let start = usize::try_from(page.cursor)
            .map_err(|_| GuardianProtocolError::InvalidCensusCursor {
                cursor: page.cursor,
                pane_count: total_panes,
            })?;
        let entries_by_bytes = (page.max_bytes - GUARDIAN_CENSUS_PAGE_HEADER_BYTES)
            / GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES;
        let page_capacity = usize::from(page.max_entries).min(
            usize::try_from(entries_by_bytes)
                .map_err(|_| GuardianProtocolError::InvalidCensusPage)?,
        );
        let entries = snapshot
            .entries
            .iter()
            .skip(start)
            .take(page_capacity)
            .cloned()
            .collect::<Vec<_>>();
        let end = start
            .checked_add(entries.len())
            .ok_or(GuardianProtocolError::CapacityExhausted)?;
        let next_cursor = if end < snapshot.entries.len() {
            Some(u64::try_from(end).map_err(|_| GuardianProtocolError::CapacityExhausted)?)
        } else {
            None
        };
        Ok(GuardianReply::CensusPage {
            snapshot_id: page.snapshot_id,
            entries,
            next_cursor,
            total_panes,
        })
    }

    fn transition_pending_input(
        &mut self,
        effect_id: Uuid,
        target: InputEffectState,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if !matches!(
            target,
            InputEffectState::DurableEffect | InputEffectState::TerminalRejected
        ) {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        let stored = self
            .effects
            .get(&effect_id)
            .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?;
        if stored.fingerprint.operation != GuardianOperation::Input {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        if stored.state == target {
            return Ok(stored.reply.clone());
        }
        if stored.state != InputEffectState::AcceptedNotDurable {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        let pane_id = stored.fingerprint.pane_id;
        let generation = stored.fingerprint.lease_generation;
        let sequence = stored.fingerprint.lease_sequence;
        let pending = match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect,
                ..
            })
            | Some(GuardianPaneState::ExitedUnclaimed {
                pending_input_effect,
                ..
            }) => *pending_input_effect,
            _ => None,
        };
        if pending != Some(effect_id) {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }

        match self.panes.get_mut(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect,
                ..
            })
            | Some(GuardianPaneState::ExitedUnclaimed {
                pending_input_effect,
                ..
            }) => *pending_input_effect = None,
            _ => return Err(GuardianProtocolError::InputDurabilityIdentityMismatch),
        }
        let reply = GuardianReply::InputReceipt {
            pane_id,
            generation,
            sequence,
            effect_id,
            state: target,
        };
        let stored = self
            .effects
            .get_mut(&effect_id)
            .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?;
        stored.state = target;
        stored.reply = reply.clone();
        let mut request_ids = self
            .effect_request_ids
            .get(&effect_id)
            .map(|request_ids| request_ids.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        request_ids.sort_unstable();
        for request_id in &request_ids {
            let Some(request) = self.requests.get_mut(request_id) else {
                continue;
            };
            request.reply = reply.clone();
        }
        // Pending input identities are deliberately not evictable. Once the
        // terminal disposition is known, every request alias and the effect
        // join the ordinary FIFO replay windows and can rotate only after the
        // generation/sequence fence makes reapplication impossible. Sort the
        // aliases first so eviction order is deterministic across processes.
        self.transient_request_order.extend(request_ids);
        self.transient_effect_order.push_back(effect_id);
        Ok(reply)
    }

    fn attach(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let pane_id = request
            .header
            .pane_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                ..
            }) if *generation == request.header.lease_generation
                && *mux_incarnation == request.header.mux_incarnation =>
            {
                Ok(GuardianReply::Attached {
                    pane_id,
                    generation: *generation,
                    next_sequence: *next_sequence,
                })
            }
            Some(GuardianPaneState::LiveUnclaimed { .. })
            | Some(GuardianPaneState::LiveClaimed { .. }) => {
                Err(GuardianProtocolError::StaleLease)
            }
            Some(_) => Err(GuardianProtocolError::PaneTerminal),
            None => Err(GuardianProtocolError::PaneNotFound(pane_id)),
        }
    }

    fn replay(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let pane_id = request
            .header
            .pane_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        let generation = match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                ..
            }) if *generation == request.header.lease_generation
                && *mux_incarnation == request.header.mux_incarnation => *generation,
            // Exit and explicit terminal retention discard mutation ownership,
            // but not authenticated transcript/checkpoint recovery authority.
            // The exact persisted generation remains the fence after census.
            Some(GuardianPaneState::ExitedUnclaimed { generation, .. })
            | Some(GuardianPaneState::ClosedTerminal { generation, .. })
            | Some(GuardianPaneState::Quarantined { generation, .. })
                if *generation == request.header.lease_generation => *generation,
            Some(_) => return Err(GuardianProtocolError::StaleLease),
            None => return Err(GuardianProtocolError::PaneNotFound(pane_id)),
        };
        Ok(GuardianReply::ReplayReady { pane_id, generation })
    }

    fn query_input_effect(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let pane_id = request
            .header
            .pane_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        self.require_effect_query_authority(pane_id, request)?;
        let effect_id = request
            .header
            .effect_id
            .ok_or(GuardianProtocolError::MissingEffectQueryIdentity)?;
        let state = self
            .effects
            .get(&effect_id)
            .filter(|stored| {
                stored.fingerprint.pane_id == pane_id
                    && stored.fingerprint.operation == GuardianOperation::Input
            })
            .map_or(InputEffectState::NotSeen, |stored| stored.state);
        Ok(GuardianReply::InputEffect { effect_id, state })
    }

    fn require_effect_query_authority(
        &self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<(), GuardianProtocolError> {
        match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                ..
            }) if *generation == request.header.lease_generation
                && *mux_incarnation == request.header.mux_incarnation => Ok(()),
            Some(GuardianPaneState::LiveUnclaimed { generation })
            | Some(GuardianPaneState::ExitedUnclaimed { generation, .. })
            | Some(GuardianPaneState::ClosedTerminal { generation, .. })
            | Some(GuardianPaneState::Quarantined { generation, .. })
                if *generation == request.header.lease_generation =>
            {
                Ok(())
            }
            Some(_) => Err(GuardianProtocolError::StaleLease),
            None => Err(GuardianProtocolError::PaneNotFound(pane_id)),
        }
    }

    fn apply_effect(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let pane_id = request
            .header
            .pane_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        let effect_id = request
            .header
            .effect_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        let fingerprint = EffectFingerprint {
            operation: request.header.operation,
            pane_id,
            mux_incarnation: request.header.mux_incarnation,
            lease_generation: request.header.lease_generation,
            lease_sequence: request.header.lease_sequence,
            payload_sha256: request.header.payload_sha256,
        };

        if let Some(stored) = self.requests.get(&request.header.request_id) {
            if stored.fingerprint == fingerprint && stored.effect_id == effect_id {
                return Ok(stored.reply.clone());
            }
            return Err(GuardianProtocolError::RequestIdentityConflict);
        }
        if let Some(stored) = self.effects.get(&effect_id) {
            if stored.fingerprint == fingerprint {
                let reply = stored.reply.clone();
                let disposition_is_pending = stored.fingerprint.operation
                    == GuardianOperation::Input
                    && stored.state == InputEffectState::AcceptedNotDurable;
                if disposition_is_pending
                    && self
                        .effect_request_ids
                        .get(&effect_id)
                        .map_or(0, HashSet::len)
                        >= GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT
                {
                    return Err(GuardianProtocolError::RequestAliasCapacityExhausted {
                        effect_id,
                        max_aliases: GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT,
                    });
                }
                self.prepare_receipt_capacity(true, false)?;
                self.make_receipt_capacity(true, false)?;
                self.requests.insert(
                    request.header.request_id,
                    StoredRequest {
                        fingerprint,
                        effect_id,
                        reply: reply.clone(),
                    },
                );
                self.effect_request_ids
                    .entry(effect_id)
                    .or_default()
                    .insert(request.header.request_id);
                if !disposition_is_pending {
                    self.transient_request_order
                        .push_back(request.header.request_id);
                }
                return Ok(reply);
            }
            return Err(GuardianProtocolError::EffectIdentityConflict);
        }
        self.prepare_receipt_capacity(true, true)?;

        let reply = match request.header.operation {
            GuardianOperation::Spawn => self.spawn(pane_id)?,
            GuardianOperation::Claim => self.claim(pane_id, request)?,
            GuardianOperation::RetireLease => self.retire_lease(pane_id, request)?,
            GuardianOperation::Input => {
                let GuardianReply::MutationApplied {
                    pane_id,
                    generation,
                    sequence,
                } = self.apply_live_mutation(pane_id, request)?
                else {
                    return Err(GuardianProtocolError::StateInvariantViolation(
                        "input-mutation-reply-variant",
                    ));
                };
                let Some(GuardianPaneState::LiveClaimed {
                    pending_input_effect,
                    ..
                }) = self.panes.get_mut(&pane_id)
                else {
                    return Err(GuardianProtocolError::StateInvariantViolation(
                        "input-claimed-pane",
                    ));
                };
                *pending_input_effect = Some(effect_id);
                GuardianReply::InputReceipt {
                    pane_id,
                    generation,
                    sequence,
                    effect_id,
                    state: InputEffectState::AcceptedNotDurable,
                }
            }
            GuardianOperation::Resize
            | GuardianOperation::Signal
            | GuardianOperation::Checkpoint => self.apply_live_mutation(pane_id, request)?,
            GuardianOperation::Close => self.close(pane_id, request)?,
            _ => {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "effect-operation-classification",
                ));
            }
        };
        self.make_receipt_capacity(true, true)?;
        self.effects.insert(
            effect_id,
            StoredEffect {
                fingerprint: fingerprint.clone(),
                reply: reply.clone(),
                state: if request.header.operation == GuardianOperation::Input {
                    InputEffectState::AcceptedNotDurable
                } else {
                    InputEffectState::DurableEffect
                },
            },
        );
        self.requests.insert(
            request.header.request_id,
            StoredRequest {
                fingerprint,
                effect_id,
                reply: reply.clone(),
            },
        );
        self.effect_request_ids
            .entry(effect_id)
            .or_default()
            .insert(request.header.request_id);
        if request.header.operation == GuardianOperation::Spawn {
            self.protected_spawn_requests
                .insert(request.header.request_id);
            self.protected_spawn_effects.insert(effect_id);
        } else if request.header.operation != GuardianOperation::Input {
            self.transient_request_order
                .push_back(request.header.request_id);
            self.transient_effect_order.push_back(effect_id);
        }
        Ok(reply)
    }

    fn prepare_receipt_capacity(
        &self,
        new_request: bool,
        new_effect: bool,
    ) -> Result<(), GuardianProtocolError> {
        if (new_request
            && self.requests.len() >= self.receipt_capacity
            && self.transient_request_order.is_empty())
            || (new_effect
                && self.effects.len() >= self.receipt_capacity
                && self.transient_effect_order.is_empty())
        {
            return Err(GuardianProtocolError::CapacityExhausted);
        }
        Ok(())
    }

    fn make_receipt_capacity(
        &mut self,
        new_request: bool,
        new_effect: bool,
    ) -> Result<(), GuardianProtocolError> {
        if new_request {
            while self.requests.len() >= self.receipt_capacity {
                let request_id = self
                    .transient_request_order
                    .pop_front()
                    .ok_or(GuardianProtocolError::CapacityExhausted)?;
                debug_assert!(!self.protected_spawn_requests.contains(&request_id));
                if let Some(request) = self.requests.remove(&request_id) {
                    if let Some(request_ids) =
                        self.effect_request_ids.get_mut(&request.effect_id)
                    {
                        request_ids.remove(&request_id);
                    }
                }
            }
        }
        if new_effect {
            while self.effects.len() >= self.receipt_capacity {
                let effect_id = self
                    .transient_effect_order
                    .pop_front()
                    .ok_or(GuardianProtocolError::CapacityExhausted)?;
                debug_assert!(!self.protected_spawn_effects.contains(&effect_id));
                self.effects.remove(&effect_id);
                self.effect_request_ids.remove(&effect_id);
            }
        }
        Ok(())
    }

    fn spawn(&mut self, pane_id: Uuid) -> Result<GuardianReply, GuardianProtocolError> {
        if self.panes.contains_key(&pane_id) {
            return Err(GuardianProtocolError::PaneAlreadyExists(pane_id));
        }
        if self.panes.len() >= GUARDIAN_MAX_PANES {
            return Err(GuardianProtocolError::CapacityExhausted);
        }
        self.panes.insert(
            pane_id,
            GuardianPaneState::LiveUnclaimed { generation: 0 },
        );
        Ok(GuardianReply::Spawned {
            pane_id,
            generation: 0,
        })
    }

    fn claim(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let state = self
            .panes
            .get_mut(&pane_id)
            .ok_or(GuardianProtocolError::PaneNotFound(pane_id))?;
        let current = state.generation();
        if current != request.header.lease_generation {
            return Err(GuardianProtocolError::ClaimGenerationMismatch {
                observed: request.header.lease_generation,
                current,
            });
        }
        if matches!(
            state,
            GuardianPaneState::ExitedUnclaimed { .. }
                | GuardianPaneState::ClosedTerminal { .. }
                | GuardianPaneState::Quarantined { .. }
        ) {
            return Err(GuardianProtocolError::PaneTerminal);
        }
        if matches!(
            state,
            GuardianPaneState::LiveClaimed {
                pending_input_effect: Some(_),
                ..
            }
        ) {
            return Err(GuardianProtocolError::InputDurabilityPending);
        }
        let Some(generation) = current.checked_add(1) else {
            *state = GuardianPaneState::Quarantined {
                generation: current,
                reason: GuardianQuarantineReason::GenerationExhausted,
                exit_status: None,
            };
            return Err(GuardianProtocolError::GenerationExhausted);
        };
        *state = GuardianPaneState::LiveClaimed {
            generation,
            mux_incarnation: request.header.mux_incarnation,
            next_sequence: 1,
            pending_input_effect: None,
        };
        Ok(GuardianReply::Claimed {
            pane_id,
            generation,
            next_sequence: 1,
        })
    }

    fn require_current_lease(
        &self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<u64, GuardianProtocolError> {
        match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                ..
            }) if *generation == request.header.lease_generation
                && *mux_incarnation == request.header.mux_incarnation => Ok(*next_sequence),
            Some(GuardianPaneState::LiveUnclaimed { .. })
            | Some(GuardianPaneState::LiveClaimed { .. }) => {
                Err(GuardianProtocolError::StaleLease)
            }
            Some(_) => Err(GuardianProtocolError::PaneTerminal),
            None => Err(GuardianProtocolError::PaneNotFound(pane_id)),
        }
    }

    fn require_exact_sequence(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<(u64, u64), GuardianProtocolError> {
        let expected = self.require_current_lease(pane_id, request)?;
        if matches!(
            self.panes.get(&pane_id),
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect: Some(_),
                ..
            })
        ) {
            return Err(GuardianProtocolError::InputDurabilityPending);
        }
        if request.header.lease_sequence < expected {
            return Err(GuardianProtocolError::RepeatedSequence {
                expected,
                observed: request.header.lease_sequence,
            });
        }
        if request.header.lease_sequence > expected {
            return Err(GuardianProtocolError::SequenceGap {
                expected,
                observed: request.header.lease_sequence,
            });
        }
        let Some(next) = expected.checked_add(1) else {
            let generation = request.header.lease_generation;
            self.panes.insert(
                pane_id,
                GuardianPaneState::Quarantined {
                    generation,
                    reason: GuardianQuarantineReason::SequenceExhausted,
                    exit_status: None,
                },
            );
            return Err(GuardianProtocolError::SequenceExhausted);
        };
        Ok((expected, next))
    }

    fn apply_live_mutation(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let (sequence, next) = self.require_exact_sequence(pane_id, request)?;
        let Some(GuardianPaneState::LiveClaimed { next_sequence, .. }) =
            self.panes.get_mut(&pane_id)
        else {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "exact-sequence-current-lease",
            ));
        };
        *next_sequence = next;
        Ok(GuardianReply::MutationApplied {
            pane_id,
            generation: request.header.lease_generation,
            sequence,
        })
    }

    fn retire_lease(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let (sequence, _next) = self.require_exact_sequence(pane_id, request)?;
        let generation = request.header.lease_generation;
        self.panes
            .insert(pane_id, GuardianPaneState::LiveUnclaimed { generation });
        let _ = sequence;
        Ok(GuardianReply::LeaseRetired {
            pane_id,
            generation,
        })
    }

    fn close(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if let Some(GuardianPaneState::ExitedUnclaimed {
            generation,
            exit_status,
            pending_input_effect,
        }) = self.panes.get(&pane_id)
        {
            if pending_input_effect.is_some() {
                return Err(GuardianProtocolError::InputDurabilityPending);
            }
            if *generation != request.header.lease_generation
                || request.header.lease_sequence != 0
            {
                return Err(GuardianProtocolError::StaleLease);
            }
            let generation = *generation;
            let exit_status = *exit_status;
            self.panes.insert(
                pane_id,
                GuardianPaneState::ClosedTerminal {
                    generation,
                    exit_status: Some(exit_status),
                },
            );
            return Ok(GuardianReply::MutationApplied {
                pane_id,
                generation,
                sequence: 0,
            });
        }
        let (sequence, _next) = self.require_exact_sequence(pane_id, request)?;
        let generation = request.header.lease_generation;
        self.panes.insert(
            pane_id,
            GuardianPaneState::ClosedTerminal {
                generation,
                exit_status: None,
            },
        );
        Ok(GuardianReply::MutationApplied {
            pane_id,
            generation,
            sequence,
        })
    }
}

pub fn encode_guardian_request(
    secret: &GuardianSecret,
    request: &GuardianRequestEnvelope,
) -> Result<Vec<u8>, GuardianProtocolError> {
    validate_request_envelope(request)?;
    let payload_len = request.payload.len();
    let frame_len = REQUEST_FRAME_HEADER_BYTES
        .checked_add(payload_len)
        .and_then(|value| value.checked_add(GUARDIAN_MAC_BYTES))
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    let total_len = FRAME_LENGTH_BYTES
        .checked_add(frame_len)
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    if total_len > GUARDIAN_MAX_FRAME_BYTES {
        return Err(GuardianProtocolError::FrameTooLarge);
    }

    let mut frame = Vec::with_capacity(total_len);
    push_u32(&mut frame, u32::try_from(frame_len).map_err(|_| GuardianProtocolError::FrameTooLarge)?);
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.extend_from_slice(&request.header.protocol_version.to_be_bytes());
    frame.push(request.header.operation as u8);
    frame.push(0);
    push_uuid(&mut frame, request.header.guardian_incarnation);
    push_uuid(&mut frame, request.header.mux_incarnation);
    push_uuid(&mut frame, request.header.request_id);
    frame.extend_from_slice(&request.header.payload_sha256);
    push_optional_uuid(&mut frame, request.header.pane_id);
    frame.extend_from_slice(&request.header.lease_generation.to_be_bytes());
    frame.extend_from_slice(&request.header.lease_sequence.to_be_bytes());
    push_optional_uuid(&mut frame, request.header.effect_id);
    push_u32(
        &mut frame,
        u32::try_from(payload_len).map_err(|_| GuardianProtocolError::PayloadTooLarge)?,
    );
    frame.extend_from_slice(&request.payload);
    let tag = secret.mac(&frame)?;
    frame.extend_from_slice(&tag);
    debug_assert_eq!(frame.len(), total_len);
    Ok(frame)
}

pub fn decode_guardian_request(
    secret: &GuardianSecret,
    frame: &[u8],
) -> Result<AuthenticatedGuardianRequest, GuardianProtocolError> {
    if frame.len() < REQUEST_FRAME_MIN_BYTES {
        return Err(GuardianProtocolError::TruncatedFrame);
    }
    if frame.len() > GUARDIAN_MAX_FRAME_BYTES {
        return Err(GuardianProtocolError::FrameTooLarge);
    }
    let declared = read_u32(frame, 0)? as usize;
    let actual = frame.len() - FRAME_LENGTH_BYTES;
    if declared != actual {
        return Err(GuardianProtocolError::FrameLengthMismatch { declared, actual });
    }
    let mac_start = frame
        .len()
        .checked_sub(GUARDIAN_MAC_BYTES)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    secret.verify(&frame[..mac_start], &frame[mac_start..])?;

    let payload_len = read_u32(frame, REQUEST_PAYLOAD_LENGTH_OFFSET)? as usize;
    if payload_len > GUARDIAN_MAX_PAYLOAD_BYTES {
        return Err(GuardianProtocolError::PayloadTooLarge);
    }
    let payload_start = FRAME_LENGTH_BYTES + REQUEST_FRAME_HEADER_BYTES;
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    if payload_end != mac_start {
        return Err(GuardianProtocolError::FrameLengthMismatch {
            declared: payload_end
                .saturating_add(GUARDIAN_MAC_BYTES)
                .saturating_sub(FRAME_LENGTH_BYTES),
            actual,
        });
    }

    if frame[4..8] != FRAME_MAGIC {
        return Err(GuardianProtocolError::InvalidMagic);
    }
    let version = read_u16(frame, 8)?;
    if version != GUARDIAN_PROTOCOL_VERSION {
        return Err(GuardianProtocolError::UnsupportedVersion(version));
    }
    let operation = GuardianOperation::from_wire(frame[10])?;
    if frame[11] != 0 {
        return Err(GuardianProtocolError::ReservedFlags);
    }
    let payload = &frame[payload_start..payload_end];
    let mut payload_sha256 = [0_u8; 32];
    payload_sha256.copy_from_slice(&frame[60..92]);
    if <[u8; 32]>::from(Sha256::digest(payload)) != payload_sha256 {
        return Err(GuardianProtocolError::PayloadDigestMismatch);
    }
    let header = GuardianRequestHeader {
        protocol_version: version,
        operation,
        guardian_incarnation: read_uuid(frame, 12)?,
        mux_incarnation: read_uuid(frame, 28)?,
        request_id: read_uuid(frame, 44)?,
        payload_sha256,
        pane_id: read_optional_uuid(frame, 92)?,
        lease_generation: read_u64(frame, 108)?,
        lease_sequence: read_u64(frame, 116)?,
        effect_id: read_optional_uuid(frame, 124)?,
    };
    let request = GuardianRequestEnvelope {
        header,
        payload: payload.to_vec(),
    };
    validate_request_envelope(&request)?;
    Ok(AuthenticatedGuardianRequest(request))
}

pub fn encode_guardian_response(
    secret: &GuardianSecret,
    response: &GuardianResponseEnvelope,
) -> Result<Vec<u8>, GuardianProtocolError> {
    validate_response_envelope(response)?;
    let payload_len = response.payload.len();
    let frame_len = RESPONSE_FRAME_HEADER_BYTES
        .checked_add(payload_len)
        .and_then(|value| value.checked_add(GUARDIAN_MAC_BYTES))
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    let total_len = FRAME_LENGTH_BYTES
        .checked_add(frame_len)
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    if total_len > GUARDIAN_MAX_FRAME_BYTES {
        return Err(GuardianProtocolError::FrameTooLarge);
    }

    let header = &response.header;
    let mut frame = Vec::with_capacity(total_len);
    push_u32(
        &mut frame,
        u32::try_from(frame_len).map_err(|_| GuardianProtocolError::FrameTooLarge)?,
    );
    frame.extend_from_slice(&RESPONSE_FRAME_MAGIC);
    frame.extend_from_slice(&header.protocol_version.to_be_bytes());
    frame.push(header.operation as u8);
    frame.push(header.status as u8);
    push_uuid(&mut frame, header.guardian_incarnation);
    push_uuid(&mut frame, header.mux_incarnation);
    push_uuid(&mut frame, header.request_id);
    frame.extend_from_slice(&header.request_payload_sha256);
    frame.extend_from_slice(&header.payload_sha256);
    push_optional_uuid(&mut frame, header.pane_id);
    frame.extend_from_slice(&header.lease_generation.to_be_bytes());
    frame.extend_from_slice(&header.lease_sequence.to_be_bytes());
    push_optional_uuid(&mut frame, header.effect_id);
    push_u32(
        &mut frame,
        u32::try_from(payload_len).map_err(|_| GuardianProtocolError::PayloadTooLarge)?,
    );
    frame.extend_from_slice(&response.payload);
    let tag = secret.mac(&frame)?;
    frame.extend_from_slice(&tag);
    debug_assert_eq!(frame.len(), total_len);
    Ok(frame)
}

pub fn decode_guardian_response(
    secret: &GuardianSecret,
    frame: &[u8],
) -> Result<AuthenticatedGuardianResponse, GuardianProtocolError> {
    if frame.len() < RESPONSE_FRAME_MIN_BYTES {
        return Err(GuardianProtocolError::TruncatedFrame);
    }
    if frame.len() > GUARDIAN_MAX_FRAME_BYTES {
        return Err(GuardianProtocolError::FrameTooLarge);
    }
    let declared = read_u32(frame, 0)? as usize;
    let actual = frame.len() - FRAME_LENGTH_BYTES;
    if declared != actual {
        return Err(GuardianProtocolError::FrameLengthMismatch { declared, actual });
    }
    let mac_start = frame
        .len()
        .checked_sub(GUARDIAN_MAC_BYTES)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    secret.verify(&frame[..mac_start], &frame[mac_start..])?;

    let payload_len = read_u32(frame, RESPONSE_PAYLOAD_LENGTH_OFFSET)? as usize;
    if payload_len > GUARDIAN_MAX_PAYLOAD_BYTES {
        return Err(GuardianProtocolError::PayloadTooLarge);
    }
    let payload_start = FRAME_LENGTH_BYTES + RESPONSE_FRAME_HEADER_BYTES;
    let payload_end = payload_start
        .checked_add(payload_len)
        .ok_or(GuardianProtocolError::FrameTooLarge)?;
    if payload_end != mac_start {
        return Err(GuardianProtocolError::FrameLengthMismatch {
            declared: payload_end
                .saturating_add(GUARDIAN_MAC_BYTES)
                .saturating_sub(FRAME_LENGTH_BYTES),
            actual,
        });
    }

    if frame[4..8] != RESPONSE_FRAME_MAGIC {
        return Err(GuardianProtocolError::InvalidMagic);
    }
    let version = read_u16(frame, 8)?;
    if version != GUARDIAN_PROTOCOL_VERSION {
        return Err(GuardianProtocolError::UnsupportedVersion(version));
    }
    let operation = GuardianOperation::from_wire(frame[10])?;
    let status = GuardianResponseStatus::from_wire(frame[11])?;
    let payload = &frame[payload_start..payload_end];
    let mut request_payload_sha256 = [0_u8; 32];
    request_payload_sha256.copy_from_slice(&frame[60..92]);
    let mut payload_sha256 = [0_u8; 32];
    payload_sha256.copy_from_slice(&frame[92..124]);
    if <[u8; 32]>::from(Sha256::digest(payload)) != payload_sha256 {
        return Err(GuardianProtocolError::PayloadDigestMismatch);
    }
    let response = GuardianResponseEnvelope {
        header: GuardianResponseHeader {
            protocol_version: version,
            operation,
            status,
            guardian_incarnation: read_uuid(frame, 12)?,
            mux_incarnation: read_uuid(frame, 28)?,
            request_id: read_uuid(frame, 44)?,
            request_payload_sha256,
            payload_sha256,
            pane_id: read_optional_uuid(frame, 124)?,
            lease_generation: read_u64(frame, 140)?,
            lease_sequence: read_u64(frame, 148)?,
            effect_id: read_optional_uuid(frame, 156)?,
        },
        payload: payload.to_vec(),
    };
    validate_response_envelope(&response)?;
    Ok(AuthenticatedGuardianResponse(response))
}

fn validate_response_envelope(
    response: &GuardianResponseEnvelope,
) -> Result<(), GuardianProtocolError> {
    let header = &response.header;
    if header.protocol_version != GUARDIAN_PROTOCOL_VERSION {
        return Err(GuardianProtocolError::UnsupportedVersion(
            header.protocol_version,
        ));
    }
    if response.payload.len() > GUARDIAN_MAX_PAYLOAD_BYTES {
        return Err(GuardianProtocolError::PayloadTooLarge);
    }
    if <[u8; 32]>::from(Sha256::digest(&response.payload)) != header.payload_sha256 {
        return Err(GuardianProtocolError::PayloadDigestMismatch);
    }
    require_nonzero(header.guardian_incarnation, "guardian incarnation")?;
    require_nonzero(header.mux_incarnation, "mux incarnation")?;
    require_nonzero(header.request_id, "request")?;
    if header.pane_id.is_some_and(|pane_id| pane_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("pane"));
    }
    if header.effect_id.is_some_and(|effect_id| effect_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("effect"));
    }
    Ok(())
}

fn validate_request_envelope(
    request: &GuardianRequestEnvelope,
) -> Result<(), GuardianProtocolError> {
    let header = &request.header;
    if header.protocol_version != GUARDIAN_PROTOCOL_VERSION {
        return Err(GuardianProtocolError::UnsupportedVersion(
            header.protocol_version,
        ));
    }
    if request.payload.len() > GUARDIAN_MAX_PAYLOAD_BYTES {
        return Err(GuardianProtocolError::PayloadTooLarge);
    }
    if <[u8; 32]>::from(Sha256::digest(&request.payload)) != header.payload_sha256 {
        return Err(GuardianProtocolError::PayloadDigestMismatch);
    }
    require_nonzero(header.guardian_incarnation, "guardian incarnation")?;
    require_nonzero(header.mux_incarnation, "mux incarnation")?;
    require_nonzero(header.request_id, "request")?;
    if header.pane_id.is_some_and(|pane_id| pane_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("pane"));
    }
    if header.effect_id.is_some_and(|effect_id| effect_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("effect"));
    }

    let operation = header.operation;
    let pane_required = operation != GuardianOperation::Census;
    let lease_required = operation.requires_lease();
    let effect_required = operation.creates_effect() || operation == GuardianOperation::QueryInputEffect;
    let spawn_scope_ok = operation != GuardianOperation::Spawn
        || (header.lease_generation == 0 && header.lease_sequence == 0);
    let census_scope_ok = operation != GuardianOperation::Census
        || (header.pane_id.is_none()
            && header.effect_id.is_none()
            && header.lease_generation == 0
            && header.lease_sequence == 0);
    let claim_scope_ok = operation != GuardianOperation::Claim || header.lease_sequence == 0;
    let sequence_scope_ok = operation.uses_mutation_sequence() || header.lease_sequence == 0;
    if pane_required != header.pane_id.is_some()
        || effect_required != header.effect_id.is_some()
        || (!lease_required
            && !matches!(operation, GuardianOperation::Spawn | GuardianOperation::Claim)
            && (header.lease_generation != 0 || header.lease_sequence != 0))
        || !spawn_scope_ok
        || !census_scope_ok
        || !claim_scope_ok
        || !sequence_scope_ok
    {
        return Err(GuardianProtocolError::InvalidOperationScope { operation });
    }
    match operation {
        GuardianOperation::Spawn if request.payload.is_empty() => {
            return Err(GuardianProtocolError::InvalidOperationScope { operation });
        }
        GuardianOperation::Census => {
            GuardianCensusPageRequest::decode(&request.payload)?;
        }
        GuardianOperation::Input
            if request.payload.is_empty() || request.payload.len() > GUARDIAN_MAX_INPUT_BYTES =>
        {
            return Err(GuardianProtocolError::InvalidOperationScope { operation });
        }
        _ => {}
    }
    Ok(())
}

fn require_nonzero(value: Uuid, label: &'static str) -> Result<(), GuardianProtocolError> {
    if value.is_nil() {
        Err(GuardianProtocolError::ZeroIdentity(label))
    } else {
        Ok(())
    }
}

fn push_uuid(buffer: &mut Vec<u8>, value: Uuid) {
    buffer.extend_from_slice(value.as_bytes());
}

fn push_optional_uuid(buffer: &mut Vec<u8>, value: Option<Uuid>) {
    push_uuid(buffer, value.unwrap_or(Uuid::nil()));
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_be_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, GuardianProtocolError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, GuardianProtocolError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, GuardianProtocolError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    Ok(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn read_uuid(bytes: &[u8], offset: usize) -> Result<Uuid, GuardianProtocolError> {
    let value = bytes
        .get(offset..offset + 16)
        .ok_or(GuardianProtocolError::TruncatedFrame)?;
    let mut uuid = [0_u8; 16];
    uuid.copy_from_slice(value);
    Ok(Uuid::from_bytes(uuid))
}

fn read_optional_uuid(bytes: &[u8], offset: usize) -> Result<Option<Uuid>, GuardianProtocolError> {
    let value = read_uuid(bytes, offset)?;
    Ok((!value.is_nil()).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn secret() -> GuardianSecret {
        GuardianSecret::from_bytes([0x5a; GUARDIAN_AUTH_TOKEN_BYTES]).unwrap()
    }

    fn request(
        operation: GuardianOperation,
        guardian: Uuid,
        mux: Uuid,
        request_id: Uuid,
        pane_id: Option<Uuid>,
        generation: u64,
        sequence: u64,
        effect_id: Option<Uuid>,
        payload: &[u8],
    ) -> GuardianRequestEnvelope {
        GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                operation,
                guardian,
                mux,
                request_id,
                pane_id,
                generation,
                sequence,
                effect_id,
                payload,
            ),
            payload.to_vec(),
        )
    }

    fn authenticate(request: &GuardianRequestEnvelope) -> AuthenticatedGuardianRequest {
        let frame = encode_guardian_request(&secret(), request).unwrap();
        decode_guardian_request(&secret(), &frame).unwrap()
    }

    fn apply_request(
        state: &mut GuardianProtocolState,
        request: &GuardianRequestEnvelope,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        state.apply(&authenticate(request))
    }

    fn spawn_request(guardian: Uuid, mux: Uuid, pane: Uuid) -> GuardianRequestEnvelope {
        request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(4),
            Some(pane),
            0,
            0,
            Some(id(5)),
            b"bounded-command",
        )
    }

    fn claim_request(
        guardian: Uuid,
        mux: Uuid,
        pane: Uuid,
        generation: u64,
        request_byte: u8,
        effect_byte: u8,
    ) -> GuardianRequestEnvelope {
        request(
            GuardianOperation::Claim,
            guardian,
            mux,
            id(request_byte),
            Some(pane),
            generation,
            0,
            Some(id(effect_byte)),
            b"",
        )
    }

    #[test]
    fn authenticated_frame_round_trip_and_tamper_rejection() {
        assert!(matches!(
            GuardianSecret::from_bytes([0; GUARDIAN_AUTH_TOKEN_BYTES]),
            Err(GuardianProtocolError::WeakSecret)
        ));
        assert_eq!(format!("{:?}", secret()), "GuardianSecret([REDACTED])");
        let original = spawn_request(id(1), id(2), id(3));
        assert!(!format!("{original:?}").contains("bounded-command"));
        let frame = encode_guardian_request(&secret(), &original).unwrap();
        assert!(frame.len() <= GUARDIAN_MAX_FRAME_BYTES);
        assert_eq!(
            decode_guardian_request(&secret(), &frame)
                .unwrap()
                .envelope(),
            &original
        );

        let mut tampered = frame.clone();
        tampered[FRAME_LENGTH_BYTES + REQUEST_FRAME_HEADER_BYTES] ^= 0x01;
        assert_eq!(
            decode_guardian_request(&secret(), &tampered),
            Err(GuardianProtocolError::AuthenticationFailed)
        );
        assert_eq!(
            decode_guardian_request(
                &GuardianSecret::from_bytes([0x6b; GUARDIAN_AUTH_TOKEN_BYTES]).unwrap(),
                &frame,
            ),
            Err(GuardianProtocolError::AuthenticationFailed)
        );
    }

    #[test]
    fn authenticated_response_round_trip_binds_request_and_effect_identity() {
        let truncated = [0_u8; RESPONSE_FRAME_MIN_BYTES - 1];
        assert_eq!(
            decode_guardian_response(&secret(), &truncated),
            Err(GuardianProtocolError::TruncatedFrame)
        );
        let original_request = spawn_request(id(1), id(2), id(3));
        let payload = b"spawned:0".to_vec();
        let response = GuardianResponseEnvelope {
            header: GuardianResponseHeader::new(
                &original_request.header,
                GuardianResponseStatus::Success,
                &payload,
            ),
            payload,
        };
        assert!(!format!("{response:?}").contains("spawned:0"));
        let frame = encode_guardian_response(&secret(), &response).unwrap();
        let authenticated = decode_guardian_response(&secret(), &frame).unwrap();
        let correlated = authenticated
            .clone()
            .correlate(&original_request.header)
            .unwrap();
        assert_eq!(correlated.envelope(), &response);

        let different_request = spawn_request(id(1), id(2), id(8));
        assert_eq!(
            authenticated.clone().correlate(&different_request.header),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );
        let mut different_payload_request = original_request.clone();
        different_payload_request.payload = b"different-command".to_vec();
        different_payload_request.header.payload_sha256 =
            Sha256::digest(&different_payload_request.payload).into();
        assert_eq!(
            authenticated
                .clone()
                .correlate(&different_payload_request.header),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );

        let mut wrong_lease = response.clone();
        wrong_lease.header.lease_generation = 1;
        let wrong_lease_frame = encode_guardian_response(&secret(), &wrong_lease).unwrap();
        let authenticated_wrong_lease =
            decode_guardian_response(&secret(), &wrong_lease_frame).unwrap();
        assert_eq!(
            authenticated_wrong_lease.correlate(&original_request.header),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );

        let mut malformed_length = frame.clone();
        malformed_length
            [RESPONSE_PAYLOAD_LENGTH_OFFSET..RESPONSE_PAYLOAD_LENGTH_OFFSET + 4]
            .copy_from_slice(&((GUARDIAN_MAX_PAYLOAD_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            decode_guardian_response(&secret(), &malformed_length),
            Err(GuardianProtocolError::AuthenticationFailed)
        );
        let mac_start = malformed_length.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&malformed_length[..mac_start]).unwrap();
        malformed_length[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_response(&secret(), &malformed_length),
            Err(GuardianProtocolError::PayloadTooLarge)
        );

        let mut tampered = frame;
        tampered[44] ^= 0x01;
        assert_eq!(
            decode_guardian_response(&secret(), &tampered),
            Err(GuardianProtocolError::AuthenticationFailed)
        );
    }

    #[test]
    fn bounded_decoder_rejects_oversize_before_authentication_or_allocation() {
        let truncated = [0_u8; REQUEST_FRAME_MIN_BYTES - 1];
        assert_eq!(
            decode_guardian_request(&secret(), &truncated),
            Err(GuardianProtocolError::TruncatedFrame)
        );
        let oversized = vec![0_u8; GUARDIAN_MAX_FRAME_BYTES + 1];
        assert_eq!(
            decode_guardian_request(&secret(), &oversized),
            Err(GuardianProtocolError::FrameTooLarge)
        );

        let mut frame = encode_guardian_request(&secret(), &spawn_request(id(1), id(2), id(3)))
            .unwrap();
        let mut wrong_outer_length = frame.clone();
        wrong_outer_length[..4].copy_from_slice(&0_u32.to_be_bytes());
        assert!(matches!(
            decode_guardian_request(&secret(), &wrong_outer_length),
            Err(GuardianProtocolError::FrameLengthMismatch { .. })
        ));
        frame[REQUEST_PAYLOAD_LENGTH_OFFSET..REQUEST_PAYLOAD_LENGTH_OFFSET + 4]
            .copy_from_slice(&((GUARDIAN_MAX_PAYLOAD_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(
            decode_guardian_request(&secret(), &frame),
            Err(GuardianProtocolError::AuthenticationFailed)
        );
        let mac_start = frame.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&frame[..mac_start]).unwrap();
        frame[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_request(&secret(), &frame),
            Err(GuardianProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn census_pagination_is_fixed_width_and_cap_checked() {
        let page = GuardianCensusPageRequest::new(
            id(90),
            0,
            GUARDIAN_MAX_CENSUS_ENTRIES,
            GUARDIAN_MAX_CENSUS_BYTES,
        )
        .unwrap();
        assert_eq!(GuardianCensusPageRequest::decode(&page.encode()).unwrap(), page);
        assert_eq!(
            GuardianCensusPageRequest::new(
                id(90),
                0,
                GUARDIAN_MAX_CENSUS_ENTRIES + 1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(id(90), 0, 0, GUARDIAN_MIN_CENSUS_PAGE_BYTES),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(
                id(90),
                0,
                1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES - 1,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(
                Uuid::nil(),
                0,
                1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::decode(
                &page.encode()[..GuardianCensusPageRequest::ENCODED_BYTES - 1],
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );

        let guardian = id(1);
        let census = request(
            GuardianOperation::Census,
            guardian,
            id(2),
            id(3),
            None,
            0,
            0,
            None,
            &page.encode(),
        );
        let frame = encode_guardian_request(&secret(), &census).unwrap();
        assert_eq!(
            decode_guardian_request(&secret(), &frame)
                .unwrap()
                .envelope(),
            &census
        );
        assert_eq!(
            apply_request(
                &mut GuardianProtocolState::new(guardian).unwrap(),
                &census,
            )
            .unwrap(),
            GuardianReply::CensusPage {
                snapshot_id: id(90),
                entries: Vec::new(),
                next_cursor: None,
                total_panes: 0,
            }
        );
    }

    #[test]
    fn census_pages_are_deterministic_complete_and_byte_bounded() {
        let guardian = id(1);
        let mux = id(2);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        for (pane_byte, request_byte, effect_byte) in
            [(40, 41, 42), (20, 21, 22), (30, 31, 32)]
        {
            let spawn = request(
                GuardianOperation::Spawn,
                guardian,
                mux,
                id(request_byte),
                Some(id(pane_byte)),
                0,
                0,
                Some(id(effect_byte)),
                b"census-pane",
            );
            apply_request(&mut state, &spawn).unwrap();
        }

        let census = |request_byte, snapshot_id, cursor, max_entries, max_bytes| {
            let page = GuardianCensusPageRequest::new(
                snapshot_id,
                cursor,
                max_entries,
                max_bytes,
            )
            .unwrap();
            request(
                GuardianOperation::Census,
                guardian,
                mux,
                id(request_byte),
                None,
                0,
                0,
                None,
                &page.encode(),
            )
        };
        let two_entry_bytes = GUARDIAN_CENSUS_PAGE_HEADER_BYTES
            + 2 * GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES;
        let snapshot_id = id(60);
        let first = apply_request(
            &mut state,
            &census(50, snapshot_id, 0, 2, two_entry_bytes),
        )
        .unwrap();
        let GuardianReply::CensusPage {
            snapshot_id: first_snapshot_id,
            entries,
            next_cursor,
            total_panes,
        } = first
        else {
            panic!("census must return a bounded page");
        };
        assert_eq!(first_snapshot_id, snapshot_id);
        assert_eq!(total_panes, 3);
        assert_eq!(next_cursor, Some(2));
        assert_eq!(
            entries.iter().map(|entry| entry.pane_id).collect::<Vec<_>>(),
            vec![id(20), id(30)]
        );
        assert!(entries.iter().all(|entry| {
            entry.status == GuardianCensusPaneStatus::LiveUnclaimed
                && entry.generation == 0
                && entry.mux_incarnation.is_none()
                && entry.next_sequence.is_none()
                && entry.pending_input_effect.is_none()
                && entry.exit_status.is_none()
                && entry.quarantine_reason.is_none()
        }));

        let conflicting_page = GuardianCensusPageRequest::new(
            snapshot_id,
            0,
            1,
            GUARDIAN_MIN_CENSUS_PAGE_BYTES,
        )
        .unwrap();
        let conflicting_mux = request(
            GuardianOperation::Census,
            guardian,
            id(62),
            id(55),
            None,
            0,
            0,
            None,
            &conflicting_page.encode(),
        );
        assert_eq!(
            apply_request(&mut state, &conflicting_mux),
            Err(GuardianProtocolError::CensusSnapshotIdentityConflict)
        );

        let concurrent_spawn = request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(43),
            Some(id(25)),
            0,
            0,
            Some(id(44)),
            b"concurrent-census-pane",
        );
        apply_request(&mut state, &concurrent_spawn).unwrap();

        let second = apply_request(
            &mut state,
            &census(
                51,
                snapshot_id,
                2,
                GUARDIAN_MAX_CENSUS_ENTRIES,
                GUARDIAN_MAX_CENSUS_BYTES,
            ),
        )
        .unwrap();
        assert_eq!(
            second,
            GuardianReply::CensusPage {
                snapshot_id,
                entries: vec![GuardianCensusEntry {
                    pane_id: id(40),
                    status: GuardianCensusPaneStatus::LiveUnclaimed,
                    generation: 0,
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    exit_status: None,
                    quarantine_reason: None,
                }],
                next_cursor: None,
                total_panes: 3,
            }
        );

        let one_by_bytes = apply_request(
            &mut state,
            &census(
                52,
                snapshot_id,
                0,
                GUARDIAN_MAX_CENSUS_ENTRIES,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            ),
        )
        .unwrap();
        assert!(matches!(
            one_by_bytes,
            GuardianReply::CensusPage {
                snapshot_id: observed_snapshot_id,
                entries,
                next_cursor: Some(1),
                total_panes: 3,
            } if observed_snapshot_id == snapshot_id
                && entries.len() == 1
                && entries[0].pane_id == id(20)
        ));
        assert_eq!(
            apply_request(
                &mut state,
                &census(
                    53,
                    snapshot_id,
                    4,
                    1,
                    GUARDIAN_MIN_CENSUS_PAGE_BYTES,
                ),
            ),
            Err(GuardianProtocolError::InvalidCensusCursor {
                cursor: 4,
                pane_count: 3,
            })
        );
        let missing_snapshot = id(61);
        assert_eq!(
            apply_request(
                &mut state,
                &census(
                    54,
                    missing_snapshot,
                    1,
                    1,
                    GUARDIAN_MIN_CENSUS_PAGE_BYTES,
                ),
            ),
            Err(GuardianProtocolError::CensusSnapshotNotFound(
                missing_snapshot,
            ))
        );
    }

    #[test]
    fn census_snapshot_cache_is_bounded_and_stale_cursors_fail_closed() {
        let guardian = id(1);
        let mux = id(2);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        for offset in 0..=GUARDIAN_MAX_CENSUS_SNAPSHOTS {
            let offset = u8::try_from(offset).expect("test snapshot offset fits u8");
            let snapshot_id = id(70 + offset);
            let page = GuardianCensusPageRequest::new(
                snapshot_id,
                0,
                1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            )
            .unwrap();
            let census = request(
                GuardianOperation::Census,
                guardian,
                mux,
                id(100 + offset),
                None,
                0,
                0,
                None,
                &page.encode(),
            );
            assert!(matches!(
                apply_request(&mut state, &census).unwrap(),
                GuardianReply::CensusPage {
                    snapshot_id: observed,
                    entries,
                    next_cursor: None,
                    total_panes: 0,
                } if observed == snapshot_id && entries.is_empty()
            ));
        }
        assert_eq!(
            state.census_snapshots.len(),
            GUARDIAN_MAX_CENSUS_SNAPSHOTS
        );
        assert_eq!(
            state.census_snapshot_order.len(),
            GUARDIAN_MAX_CENSUS_SNAPSHOTS
        );

        let retired_snapshot = id(70);
        let stale_page = GuardianCensusPageRequest::new(
            retired_snapshot,
            1,
            1,
            GUARDIAN_MIN_CENSUS_PAGE_BYTES,
        )
        .unwrap();
        let stale_census = request(
            GuardianOperation::Census,
            guardian,
            mux,
            id(120),
            None,
            0,
            0,
            None,
            &stale_page.encode(),
        );
        assert_eq!(
            apply_request(&mut state, &stale_census),
            Err(GuardianProtocolError::CensusSnapshotNotFound(
                retired_snapshot,
            ))
        );
    }

    #[test]
    fn spawn_is_exactly_idempotent_and_conflicts_fail_before_second_effect() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        let spawn = spawn_request(guardian, mux, pane);
        let first = apply_request(&mut state, &spawn).unwrap();
        assert_eq!(apply_request(&mut state, &spawn).unwrap(), first);
        let mut same_spawn_effect_after_ambiguous_reply = spawn.clone();
        same_spawn_effect_after_ambiguous_reply.header.request_id = id(6);
        assert_eq!(
            apply_request(&mut state, &same_spawn_effect_after_ambiguous_reply).unwrap(),
            first
        );
        assert_eq!(state.panes.len(), 1);

        let mut conflicting = spawn.clone();
        conflicting.payload = b"different-command".to_vec();
        conflicting.header.payload_sha256 = Sha256::digest(&conflicting.payload).into();
        assert_eq!(
            apply_request(&mut state, &conflicting),
            Err(GuardianProtocolError::RequestIdentityConflict)
        );
        assert_eq!(state.panes.len(), 1);
    }

    #[test]
    fn claims_monotonically_fence_old_mux_and_reject_skips() {
        let guardian = id(1);
        let old_mux = id(2);
        let new_mux = id(7);
        let pane = id(3);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, old_mux, pane)).unwrap();
        let first_claim = claim_request(guardian, old_mux, pane, 0, 6, 7);
        assert_eq!(
            apply_request(&mut state, &first_claim).unwrap(),
            GuardianReply::Claimed {
                pane_id: pane,
                generation: 1,
                next_sequence: 1,
            }
        );

        let skipped = claim_request(guardian, new_mux, pane, 2, 8, 9);
        assert_eq!(
            apply_request(&mut state, &skipped),
            Err(GuardianProtocolError::ClaimGenerationMismatch {
                observed: 2,
                current: 1,
            })
        );
        let successor = claim_request(guardian, new_mux, pane, 1, 10, 11);
        assert_eq!(
            apply_request(&mut state, &successor).unwrap(),
            GuardianReply::Claimed {
                pane_id: pane,
                generation: 2,
                next_sequence: 1,
            }
        );

        let stale = request(
            GuardianOperation::Input,
            guardian,
            old_mux,
            id(12),
            Some(pane),
            1,
            1,
            Some(id(13)),
            b"must-not-apply",
        );
        assert_eq!(
            apply_request(&mut state, &stale),
            Err(GuardianProtocolError::StaleLease)
        );
        assert!(!state.effects.contains_key(&id(13)));
    }

    #[test]
    fn read_only_lease_requests_use_zero_without_consuming_mutation_sequence() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let attach = request(
            GuardianOperation::Attach,
            guardian,
            mux,
            id(8),
            Some(pane),
            1,
            0,
            None,
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &attach).unwrap(),
            GuardianReply::Attached {
                pane_id: pane,
                generation: 1,
                next_sequence: 1,
            }
        );
        assert_eq!(
            apply_request(&mut state, &attach).unwrap(),
            GuardianReply::Attached {
                pane_id: pane,
                generation: 1,
                next_sequence: 1,
            }
        );

        let mut invalid_attach = attach;
        invalid_attach.header.request_id = id(9);
        invalid_attach.header.lease_sequence = 1;
        assert_eq!(
            encode_guardian_request(&secret(), &invalid_attach),
            Err(GuardianProtocolError::InvalidOperationScope {
                operation: GuardianOperation::Attach,
            })
        );

        let resize = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(10),
            Some(pane),
            1,
            1,
            Some(id(11)),
            b"100x30",
        );
        assert_eq!(
            apply_request(&mut state, &resize).unwrap(),
            GuardianReply::MutationApplied {
                pane_id: pane,
                generation: 1,
                sequence: 1,
            }
        );
    }

    #[test]
    fn input_effect_is_queryable_and_never_blindly_replayed() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(20);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(21),
            Some(pane),
            1,
            1,
            Some(effect),
            b"hello",
        );
        let receipt = apply_request(&mut state, &input).unwrap();
        assert_eq!(
            receipt,
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::AcceptedNotDurable,
            }
        );
        assert_eq!(apply_request(&mut state, &input).unwrap(), receipt);
        let mut retry_after_ambiguous_response = input.clone();
        retry_after_ambiguous_response.header.request_id = id(24);
        assert_eq!(
            apply_request(&mut state, &retry_after_ambiguous_response).unwrap(),
            receipt
        );
        assert_eq!(state.effect_request_ids[&effect].len(), 2);
        assert!(
            !state.transient_effect_order.contains(&effect),
            "an input whose disposition is unknown must not be evictable"
        );
        assert!(
            !state.transient_request_order.contains(&input.header.request_id)
                && !state
                    .transient_request_order
                    .contains(&retry_after_ambiguous_response.header.request_id),
            "every request alias for ambiguous input must remain pinned"
        );

        let query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(22),
            Some(pane),
            1,
            0,
            Some(effect),
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::AcceptedNotDurable,
            }
        );

        let mut conflicting = input.clone();
        conflicting.header.request_id = id(23);
        conflicting.payload = b"different".to_vec();
        conflicting.header.payload_sha256 = Sha256::digest(&conflicting.payload).into();
        assert_eq!(
            apply_request(&mut state, &conflicting),
            Err(GuardianProtocolError::EffectIdentityConflict)
        );

        let successor = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(25),
            Some(pane),
            1,
            2,
            Some(id(26)),
            b"120x40",
        );
        assert_eq!(
            apply_request(&mut state, &successor),
            Err(GuardianProtocolError::InputDurabilityPending)
        );
        let premature_takeover = claim_request(guardian, id(31), pane, 1, 32, 33);
        assert_eq!(
            apply_request(&mut state, &premature_takeover),
            Err(GuardianProtocolError::InputDurabilityPending)
        );
        assert_eq!(
            state.mark_input_durable(effect).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableEffect,
            }
        );
        assert_eq!(
            apply_request(&mut state, &retry_after_ambiguous_response).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableEffect,
            }
        );
        assert_eq!(
            apply_request(&mut state, &input).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableEffect,
            },
            "durability acknowledgement must update every retained alias"
        );
        assert!(state.transient_effect_order.contains(&effect));
        assert!(
            state.transient_request_order.contains(&input.header.request_id)
                && state
                    .transient_request_order
                    .contains(&retry_after_ambiguous_response.header.request_id),
            "terminal input disposition must make every retained alias evictable"
        );
        assert_eq!(
            apply_request(&mut state, &query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::DurableEffect,
            }
        );
        assert!(apply_request(&mut state, &successor).is_ok());

        let close = request(
            GuardianOperation::Close,
            guardian,
            mux,
            id(27),
            Some(pane),
            1,
            3,
            Some(id(28)),
            b"explicit-close",
        );
        assert!(apply_request(&mut state, &close).is_ok());
        let terminal_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            id(29),
            id(30),
            Some(pane),
            1,
            0,
            Some(effect),
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &terminal_query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::DurableEffect,
            }
        );
        state.mark_exited(pane, 129).unwrap();
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::ClosedTerminal {
                generation: 1,
                exit_status: Some(129),
            })
        ));
    }

    #[test]
    fn sequence_gap_and_repetition_have_zero_effect() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let skipped = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(30),
            Some(pane),
            1,
            2,
            Some(id(31)),
            b"80x24",
        );
        assert_eq!(
            apply_request(&mut state, &skipped),
            Err(GuardianProtocolError::SequenceGap {
                expected: 1,
                observed: 2,
            })
        );
        assert!(!state.effects.contains_key(&id(31)));

        let first = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(32),
            Some(pane),
            1,
            1,
            Some(id(33)),
            b"80x24",
        );
        apply_request(&mut state, &first).unwrap();
        let repeated = request(
            GuardianOperation::Signal,
            guardian,
            mux,
            id(34),
            Some(pane),
            1,
            1,
            Some(id(35)),
            b"TERM",
        );
        assert_eq!(
            apply_request(&mut state, &repeated),
            Err(GuardianProtocolError::RepeatedSequence {
                expected: 2,
                observed: 1,
            })
        );
        assert!(!state.effects.contains_key(&id(35)));
    }

    #[test]
    fn pending_input_aliases_cannot_monopolize_the_global_receipt_ledger() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(20);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(21),
            Some(pane),
            1,
            1,
            Some(effect),
            b"ambiguous-input",
        );
        let pending_receipt = apply_request(&mut state, &input).unwrap();
        for alias_number in 1..GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT {
            let mut alias = input.clone();
            alias.header.request_id = Uuid::from_u128(0x1_0000 + alias_number as u128);
            assert_eq!(apply_request(&mut state, &alias).unwrap(), pending_receipt);
        }
        assert_eq!(
            state.effect_request_ids[&effect].len(),
            GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT
        );

        let mut rejected_alias = input.clone();
        rejected_alias.header.request_id = Uuid::from_u128(0x2_0000);
        assert_eq!(
            apply_request(&mut state, &rejected_alias),
            Err(GuardianProtocolError::RequestAliasCapacityExhausted {
                effect_id: effect,
                max_aliases: GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT,
            })
        );
        assert!(!state.requests.contains_key(&rejected_alias.header.request_id));
        assert_eq!(
            state.effect_request_ids[&effect].len(),
            GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT,
            "rejection must not mutate the retained alias set"
        );
        assert_eq!(apply_request(&mut state, &input).unwrap(), pending_receipt);
        assert!(matches!(
            state.mark_input_durable(effect).unwrap(),
            GuardianReply::InputReceipt {
                state: InputEffectState::DurableEffect,
                ..
            }
        ));
    }

    #[test]
    fn child_exit_preserves_pending_input_reconciliation_and_terminal_close() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(40);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();
        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(41),
            Some(pane),
            1,
            1,
            Some(effect),
            b"exit-after-input",
        );
        apply_request(&mut state, &input).unwrap();
        state.mark_exited(pane, 0).unwrap();
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::ExitedUnclaimed {
                pending_input_effect: Some(value),
                ..
            }) if *value == effect
        ));
        let replay = request(
            GuardianOperation::Replay,
            guardian,
            id(42),
            id(45),
            Some(pane),
            1,
            0,
            None,
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &replay).unwrap(),
            GuardianReply::ReplayReady {
                pane_id: pane,
                generation: 1,
            },
            "child exit must retain authenticated replay authority"
        );
        let mut stale_replay = replay;
        stale_replay.header.request_id = id(46);
        stale_replay.header.lease_generation = 0;
        assert_eq!(
            apply_request(&mut state, &stale_replay),
            Err(GuardianProtocolError::StaleLease)
        );
        let close = request(
            GuardianOperation::Close,
            guardian,
            id(42),
            id(43),
            Some(pane),
            1,
            0,
            Some(id(44)),
            b"retention-close",
        );
        assert_eq!(
            apply_request(&mut state, &close),
            Err(GuardianProtocolError::InputDurabilityPending)
        );
        state.mark_input_durable(effect).unwrap();
        assert_eq!(
            apply_request(&mut state, &close).unwrap(),
            GuardianReply::MutationApplied {
                pane_id: pane,
                generation: 1,
                sequence: 0,
            }
        );
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::ClosedTerminal {
                generation: 1,
                exit_status: Some(0),
            })
        ));
    }

    #[test]
    fn definitively_unapplied_input_can_be_terminally_rejected_by_exact_identity() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(50);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();
        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(51),
            Some(pane),
            1,
            1,
            Some(effect),
            b"journal-first-input",
        );
        apply_request(&mut state, &input).unwrap();
        assert_eq!(
            state.mark_input_terminal_rejected(effect).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::TerminalRejected,
            }
        );
        assert_eq!(
            state.mark_input_durable(effect),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch)
        );
        let successor = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(52),
            Some(pane),
            1,
            2,
            Some(id(53)),
            b"100x30",
        );
        assert!(apply_request(&mut state, &successor).is_ok());
    }

    #[test]
    fn bounded_receipt_window_rotates_mutations_but_never_forgets_original_spawn() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new_with_receipt_capacity(guardian, 3).unwrap();
        let spawn = spawn_request(guardian, mux, pane);
        let spawned = apply_request(&mut state, &spawn).unwrap();
        let claim = claim_request(guardian, mux, pane, 0, 6, 7);
        apply_request(&mut state, &claim).unwrap();

        let resize = |request_byte, effect_byte, sequence| {
            request(
                GuardianOperation::Resize,
                guardian,
                mux,
                id(request_byte),
                Some(pane),
                1,
                sequence,
                Some(id(effect_byte)),
                b"120x40",
            )
        };
        let resize_one = resize(8, 9, 1);
        apply_request(&mut state, &resize_one).unwrap();
        apply_request(&mut state, &resize(10, 11, 2)).unwrap();
        apply_request(&mut state, &resize(12, 13, 3)).unwrap();

        assert_eq!(state.requests.len(), 3);
        assert_eq!(state.effects.len(), 3);
        assert!(state.requests.contains_key(&spawn.header.request_id));
        assert!(state.effects.contains_key(&spawn.header.effect_id.unwrap()));
        assert!(!state.requests.contains_key(&claim.header.request_id));
        assert!(!state.effects.contains_key(&claim.header.effect_id.unwrap()));
        assert!(!state.requests.contains_key(&resize_one.header.request_id));
        assert!(!state.effects.contains_key(&resize_one.header.effect_id.unwrap()));

        assert_eq!(apply_request(&mut state, &spawn).unwrap(), spawned);
        assert_eq!(
            apply_request(&mut state, &claim),
            Err(GuardianProtocolError::ClaimGenerationMismatch {
                observed: 0,
                current: 1,
            })
        );
        assert_eq!(
            apply_request(&mut state, &resize_one),
            Err(GuardianProtocolError::RepeatedSequence {
                expected: 4,
                observed: 1,
            })
        );
        assert!(apply_request(&mut state, &resize(14, 15, 4)).is_ok());
        assert_eq!(state.requests.len(), 3);
        assert_eq!(state.effects.len(), 3);
    }

    #[test]
    fn receipt_pressure_pins_ambiguous_input_until_its_terminal_disposition() {
        let guardian = id(1);
        let mux = id(2);
        let first_pane = id(3);
        let second_pane = id(30);
        let input_effect = id(9);
        let mut state = GuardianProtocolState::new_with_receipt_capacity(guardian, 4).unwrap();

        apply_request(&mut state, &spawn_request(guardian, mux, first_pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, first_pane, 0, 6, 7),
        )
        .unwrap();
        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(8),
            Some(first_pane),
            1,
            1,
            Some(input_effect),
            b"must-survive-receipt-pressure",
        );
        apply_request(&mut state, &input).unwrap();

        let second_spawn = request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(10),
            Some(second_pane),
            0,
            0,
            Some(id(11)),
            b"second-bounded-command",
        );
        apply_request(&mut state, &second_spawn).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, second_pane, 0, 12, 13),
        )
        .unwrap();

        let resize = |request_byte, effect_byte, sequence| {
            request(
                GuardianOperation::Resize,
                guardian,
                mux,
                id(request_byte),
                Some(second_pane),
                1,
                sequence,
                Some(id(effect_byte)),
                b"90x28",
            )
        };
        apply_request(&mut state, &resize(14, 15, 1)).unwrap();
        apply_request(&mut state, &resize(16, 17, 2)).unwrap();

        assert_eq!(state.effects.len(), 4);
        assert!(state.effects.contains_key(&input_effect));
        assert!(!state.transient_effect_order.contains(&input_effect));
        assert!(matches!(
            state.pane_state(first_pane),
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect: Some(effect),
                ..
            }) if *effect == input_effect
        ));

        state.mark_input_durable(input_effect).unwrap();
        assert!(state.transient_effect_order.contains(&input_effect));
        apply_request(&mut state, &resize(18, 19, 3)).unwrap();
        assert!(state.effects.contains_key(&input_effect));
        apply_request(&mut state, &resize(20, 21, 4)).unwrap();
        assert!(
            !state.effects.contains_key(&input_effect),
            "a resolved input may rotate only after its sequence fence is durable"
        );
        assert_eq!(
            apply_request(&mut state, &input),
            Err(GuardianProtocolError::RepeatedSequence {
                expected: 2,
                observed: 1,
            }),
            "receipt eviction must never permit a second input effect"
        );
    }

    #[test]
    fn guardian_incarnation_rollover_invalidates_old_requests() {
        let request = spawn_request(id(1), id(2), id(3));
        let mut successor = GuardianProtocolState::new(id(9)).unwrap();
        assert_eq!(
            apply_request(&mut successor, &request),
            Err(GuardianProtocolError::GuardianIncarnationMismatch)
        );
        assert!(successor.panes.is_empty());
    }

    #[test]
    fn generation_and_sequence_exhaustion_quarantine_instead_of_wrapping() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut generation_state = GuardianProtocolState::new(guardian).unwrap();
        generation_state.panes.insert(
            pane,
            GuardianPaneState::LiveUnclaimed {
                generation: u64::MAX,
            },
        );
        assert_eq!(
            apply_request(
                &mut generation_state,
                &claim_request(guardian, mux, pane, u64::MAX, 40, 41),
            ),
            Err(GuardianProtocolError::GenerationExhausted)
        );
        assert!(matches!(
            generation_state.pane_state(pane),
            Some(GuardianPaneState::Quarantined {
                reason: GuardianQuarantineReason::GenerationExhausted,
                ..
            })
        ));

        let mut sequence_state = GuardianProtocolState::new(guardian).unwrap();
        sequence_state.panes.insert(
            pane,
            GuardianPaneState::LiveClaimed {
                generation: 9,
                mux_incarnation: mux,
                next_sequence: u64::MAX,
                pending_input_effect: None,
            },
        );
        let exhausted = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(42),
            Some(pane),
            9,
            u64::MAX,
            Some(id(43)),
            b"never-applied",
        );
        assert_eq!(
            apply_request(&mut sequence_state, &exhausted),
            Err(GuardianProtocolError::SequenceExhausted)
        );
        assert!(!sequence_state.effects.contains_key(&id(43)));
        assert!(matches!(
            sequence_state.pane_state(pane),
            Some(GuardianPaneState::Quarantined {
                reason: GuardianQuarantineReason::SequenceExhausted,
                ..
            })
        ));
        sequence_state.mark_exited(pane, 137).unwrap();
        assert!(matches!(
            sequence_state.pane_state(pane),
            Some(GuardianPaneState::Quarantined {
                reason: GuardianQuarantineReason::SequenceExhausted,
                exit_status: Some(137),
                ..
            })
        ));
        assert_eq!(
            sequence_state.mark_exited(pane, 137),
            Err(GuardianProtocolError::PaneTerminal)
        );
    }
}
