//! Authenticated, bounded protocol and pure fencing state machine for the PTY guardian.
//!
//! This module deliberately contains no sockets, PTYs, subprocesses, or mux-global lookups.
//! A transport must decode and authenticate a complete frame here before it is allowed to
//! route the request to a pane runtime.  The pure state machine is the authority for spawn
//! idempotency, lease generations, mutation sequencing, ambiguous input reconciliation, and
//! typed durable-checkpoint publication fences.
//! A fresh mux first uses the authenticated `Hello` operation to learn the current guardian
//! incarnation; nil incarnation scope is otherwise forbidden.

use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use hmac::{Hmac, KeyInit, Mac};
use portable_pty::{PtySize, cmdbuilder::CommandBuilder};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::panic::AssertUnwindSafe;
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const GUARDIAN_PROTOCOL_VERSION: u16 = 4;
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
/// Maximum plaintext carried by one checkpoint upload chunk or replay page.
///
/// This leaves ample space beneath the authenticated 512-KiB payload ceiling
/// for fixed metadata and the maximum 32 output-record descriptors.
pub const GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES: u32 = 256 * 1024;
pub const GUARDIAN_MAX_REPLAY_RECORDS: u16 = 32;
pub const GUARDIAN_MAX_REPLAY_WAIT_MILLIS: u16 = 1_000;
pub const GUARDIAN_MAX_CHECKPOINT_BYTES: u64 = 256 * 1024 * 1024;
pub const GUARDIAN_MAX_CHECKPOINT_CHUNKS: u32 = 1_024;
pub const GUARDIAN_MAX_REPLAY_SNAPSHOTS_PER_CONNECTION: usize = 8;
pub const GUARDIAN_MAX_REPLAY_SNAPSHOTS_PER_SERVICE: usize = 64;
pub const GUARDIAN_CENSUS_PAGE_HEADER_BYTES: u32 = 34;
pub const GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES: u32 = 87;
pub const GUARDIAN_MIN_CENSUS_PAGE_BYTES: u32 =
    GUARDIAN_CENSUS_PAGE_HEADER_BYTES + GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES;
pub const GUARDIAN_CHECKPOINT_INTENT_VERSION: u16 = 1;
pub const GUARDIAN_CHECKPOINT_INTENT_BYTES: usize = 72;
pub const GUARDIAN_CHECKPOINT_RECEIPT_BYTES: usize = 120;

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
const SPAWN_PAYLOAD_MAGIC: [u8; 4] = *b"GSP1";
const RESIZE_PAYLOAD_MAGIC: [u8; 4] = *b"GRS1";
const SIGNAL_PAYLOAD_MAGIC: [u8; 4] = *b"GSG1";
const INPUT_EFFECT_QUERY_PAYLOAD_MAGIC: [u8; 4] = *b"GIQ1";
const CHECKPOINT_INTENT_PAYLOAD_MAGIC: [u8; 4] = *b"GCP1";
const CHECKPOINT_RECEIPT_PAYLOAD_MAGIC: [u8; 4] = *b"GCR1";
const CHECKPOINT_STAGE_PAYLOAD_MAGIC: [u8; 4] = *b"GCS1";
const CHECKPOINT_STAGE_REPLY_MAGIC: [u8; 4] = *b"GSR1";
const REPLAY_REQUEST_PAYLOAD_MAGIC: [u8; 4] = *b"GRQ1";
const REPLAY_PAGE_PAYLOAD_MAGIC: [u8; 4] = *b"GRP1";
const REPLAY_ACK_PAYLOAD_MAGIC: [u8; 4] = *b"GRA1";
const REPLAY_ACK_REPLY_MAGIC: [u8; 4] = *b"GAR1";
const REJECTION_PAYLOAD_MAGIC: [u8; 4] = *b"GRE1";
const SPAWN_PAYLOAD_FIXED_BYTES: usize = 16;
const RESIZE_PAYLOAD_BYTES: usize = 12;
const SIGNAL_PAYLOAD_BYTES: usize = 5;
const INPUT_EFFECT_QUERY_PAYLOAD_BYTES: usize = 48;
const INPUT_RECEIPT_PAYLOAD_BYTES: usize = 53;
const INPUT_EFFECT_REPLY_PAYLOAD_BYTES: usize = 21;
const REJECTION_PAYLOAD_BYTES: usize = 6;
const CHECKPOINT_STAGE_SCOPE_BYTES: usize = 32;
const CHECKPOINT_STAGE_COMMON_BYTES: usize = 136;
const CHECKPOINT_STAGE_CHUNK_FIXED_BYTES: usize = CHECKPOINT_STAGE_COMMON_BYTES + 48;
const CHECKPOINT_STAGE_REPLY_BYTES: usize = 100;
const REPLAY_CURSOR_BYTES: usize = 160;
const REPLAY_OPEN_REQUEST_BYTES: usize = 92;
const REPLAY_CONTINUE_REQUEST_BYTES: usize = 8 + REPLAY_CURSOR_BYTES;
const REPLAY_ACK_BYTES: usize = 168;
const REPLAY_ACK_REPLY_BYTES: usize = 132;
const REPLAY_PAGE_HEADER_BYTES: usize = 316;
const REPLAY_PAGE_DIGEST_OFFSET: usize = 120;
const REPLAY_PAGE_DIGEST_END: usize = REPLAY_PAGE_DIGEST_OFFSET + 32;
const REPLAY_CHECKPOINT_DESCRIPTOR_BYTES: usize = 224;
const REPLAY_CHECKPOINT_CHUNK_FIXED_BYTES: usize = REPLAY_CHECKPOINT_DESCRIPTOR_BYTES + 44;
const REPLAY_OUTPUT_RECORD_FIXED_BYTES: usize = 168;
const REPLAY_COMPLETE_BYTES: usize = 80;
const REPLAY_GAP_BYTES: usize = 32;
const REPLAY_COMPACTED_BYTES: usize = 32 + REPLAY_CHECKPOINT_DESCRIPTOR_BYTES + 16;
const REPLAY_SNAPSHOT_EXPIRED_BYTES: usize = 16;
const CHECKPOINT_STAGE_WIRE_VERSION: u16 = 1;
const REPLAY_WIRE_VERSION: u16 = 1;
const REPLAY_CURSOR_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian.replay-cursor.v1";
const REPLAY_PAGE_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian.replay-page.v1";

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
    /// Authenticated bootstrap that discovers the current guardian
    /// incarnation before any pane-scoped request can be formed.
    Hello = 13,
    /// Authenticated process-scoped request to stop an empty guardian.
    ///
    /// Admission and response-flush ordering are owned by the guardian
    /// transport; the pane protocol state machine deliberately cannot apply
    /// this effect.
    GuardedStop = 14,
    /// Content-addressed, bounded checkpoint upload staging. This never
    /// consumes a PTY mutation sequence; `Checkpoint` performs publication.
    CheckpointStage = 15,
    /// Cumulative acknowledgement for one deterministic replay page.
    ReplayAck = 16,
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
            13 => Ok(Self::Hello),
            14 => Ok(Self::GuardedStop),
            15 => Ok(Self::CheckpointStage),
            16 => Ok(Self::ReplayAck),
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
                | Self::GuardedStop
        )
    }

    const fn supports_generic_effect_indeterminate(self) -> bool {
        matches!(
            self,
            Self::Spawn
                | Self::Claim
                | Self::Resize
                | Self::Signal
                | Self::Close
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
                | Self::ReplayAck
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

#[derive(Clone, Eq, PartialEq)]
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

impl std::fmt::Debug for GuardianRequestHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianRequestHeader")
            .field("protocol_version", &self.protocol_version)
            .field("operation", &self.operation)
            .field("guardian_incarnation", &self.guardian_incarnation)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("request_id", &self.request_id)
            .field("pane_id", &self.pane_id)
            .field("lease_generation", &self.lease_generation)
            .field("lease_sequence", &self.lease_sequence)
            .field("effect_id", &self.effect_id)
            .finish_non_exhaustive()
    }
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
    payload: Vec<u8>,
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

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Wipe the owned payload immediately while retaining its authenticated
    /// header for content-free correlation and diagnostics.
    pub fn zeroize_payload(&mut self) {
        self.payload.zeroize();
    }
}

impl Drop for GuardianRequestEnvelope {
    fn drop(&mut self) {
        self.payload.zeroize();
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthenticatedGuardianRequest {
    envelope: GuardianRequestEnvelope,
    /// Authenticated length retained independently of the wipeable plaintext.
    ///
    /// Input terminal receipts need this bound after the live writer has
    /// consumed and zeroized the payload.  Retaining only the length and the
    /// header's authenticated digest preserves response correlation without a
    /// second plaintext allocation.
    authenticated_payload_bytes: u32,
}

impl std::fmt::Debug for AuthenticatedGuardianRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("AuthenticatedGuardianRequest")
            .field(&self.envelope)
            .finish()
    }
}

impl std::ops::Deref for AuthenticatedGuardianRequest {
    type Target = GuardianRequestEnvelope;

    fn deref(&self) -> &Self::Target {
        &self.envelope
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianResponseStatus {
    Success = 0,
    Rejected = 1,
    Terminal = 2,
    /// The request was authenticated and executed, but durable publication
    /// may have occurred and therefore cannot be retried blindly.
    Indeterminate = 3,
}

impl GuardianResponseStatus {
    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            0 => Ok(Self::Success),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Terminal),
            3 => Ok(Self::Indeterminate),
            other => Err(GuardianProtocolError::UnknownResponseStatus(other)),
        }
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianRejectionCode {
    InvalidRequest = 1,
    GuardianIncarnationMismatch = 2,
    PaneNotFound = 3,
    PaneAlreadyExists = 4,
    RequestIdentityConflict = 5,
    EffectIdentityConflict = 6,
    PaneTerminal = 7,
    ClaimGenerationMismatch = 8,
    StaleLease = 9,
    RepeatedSequence = 10,
    SequenceGap = 11,
    GenerationExhausted = 12,
    SequenceExhausted = 13,
    CapacityExhausted = 14,
    RequestAliasCapacityExhausted = 15,
    InputDurabilityPending = 16,
    InputDurabilityIdentityMismatch = 17,
    CensusSnapshotNotFound = 18,
    CensusSnapshotIdentityConflict = 19,
    InvalidCensusCursor = 20,
    InternalInvariant = 21,
    CheckpointOutcomeIndeterminate = 22,
    CheckpointIdentityMismatch = 23,
    OwnedPanesPresent = 24,
    /// The exact input effect durably proved that zero bytes reached the PTY.
    InputKnownNotApplied = 25,
}

impl GuardianRejectionCode {
    #[must_use]
    pub const fn status(self) -> GuardianResponseStatus {
        match self {
            Self::PaneNotFound
            | Self::SequenceGap
            | Self::CapacityExhausted
            | Self::RequestAliasCapacityExhausted
            | Self::InputDurabilityPending
            | Self::CheckpointOutcomeIndeterminate
            | Self::OwnedPanesPresent => GuardianResponseStatus::Rejected,
            _ => GuardianResponseStatus::Terminal,
        }
    }

    #[must_use]
    pub fn encode(self) -> [u8; REJECTION_PAYLOAD_BYTES] {
        let mut payload = [0_u8; REJECTION_PAYLOAD_BYTES];
        payload[..4].copy_from_slice(&REJECTION_PAYLOAD_MAGIC);
        payload[4..].copy_from_slice(&(self as u16).to_be_bytes());
        payload
    }

    pub fn decode(
        status: GuardianResponseStatus,
        payload: &[u8],
    ) -> Result<Self, GuardianProtocolError> {
        if matches!(
            status,
            GuardianResponseStatus::Success | GuardianResponseStatus::Indeterminate
        )
            || payload.len() != REJECTION_PAYLOAD_BYTES
            || payload.get(..4) != Some(REJECTION_PAYLOAD_MAGIC.as_slice())
        {
            return Err(GuardianProtocolError::InvalidRejectionPayload);
        }
        let code = match read_u16(payload, 4)? {
            1 => Self::InvalidRequest,
            2 => Self::GuardianIncarnationMismatch,
            3 => Self::PaneNotFound,
            4 => Self::PaneAlreadyExists,
            5 => Self::RequestIdentityConflict,
            6 => Self::EffectIdentityConflict,
            7 => Self::PaneTerminal,
            8 => Self::ClaimGenerationMismatch,
            9 => Self::StaleLease,
            10 => Self::RepeatedSequence,
            11 => Self::SequenceGap,
            12 => Self::GenerationExhausted,
            13 => Self::SequenceExhausted,
            14 => Self::CapacityExhausted,
            15 => Self::RequestAliasCapacityExhausted,
            16 => Self::InputDurabilityPending,
            17 => Self::InputDurabilityIdentityMismatch,
            18 => Self::CensusSnapshotNotFound,
            19 => Self::CensusSnapshotIdentityConflict,
            20 => Self::InvalidCensusCursor,
            21 => Self::InternalInvariant,
            22 => Self::CheckpointOutcomeIndeterminate,
            23 => Self::CheckpointIdentityMismatch,
            24 => Self::OwnedPanesPresent,
            25 => Self::InputKnownNotApplied,
            _ => return Err(GuardianProtocolError::InvalidRejectionPayload),
        };
        if code.status() != status {
            return Err(GuardianProtocolError::InvalidRejectionPayload);
        }
        Ok(code)
    }
}

#[derive(Clone, Eq, PartialEq)]
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

impl std::fmt::Debug for GuardianResponseHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianResponseHeader")
            .field("protocol_version", &self.protocol_version)
            .field("operation", &self.operation)
            .field("status", &self.status)
            .field("guardian_incarnation", &self.guardian_incarnation)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("request_id", &self.request_id)
            .field("pane_id", &self.pane_id)
            .field("lease_generation", &self.lease_generation)
            .field("lease_sequence", &self.lease_sequence)
            .field("effect_id", &self.effect_id)
            .finish_non_exhaustive()
    }
}

impl GuardianResponseHeader {
    #[must_use]
    fn new(
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
    header: GuardianResponseHeader,
    payload: Vec<u8>,
}

impl GuardianResponseEnvelope {
    #[must_use]
    pub const fn header(&self) -> &GuardianResponseHeader {
        &self.header
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn reply(
        request: &AuthenticatedGuardianRequest,
        reply: &GuardianReply,
    ) -> Result<Self, GuardianProtocolError> {
        let payload = reply.encode_for_operation(request.header.operation)?;
        let response = Self {
            header: GuardianResponseHeader::new(
                &request.header,
                reply.response_status(),
                &payload,
            ),
            payload,
        };
        reply.require_response_identity(&response.header)?;
        reply.require_request_payload(request)?;
        Ok(response)
    }

    pub fn success(
        request: &AuthenticatedGuardianRequest,
        reply: &GuardianReply,
    ) -> Result<Self, GuardianProtocolError> {
        let response = Self::reply(request, reply)?;
        if response.header.status == GuardianResponseStatus::Success {
            Ok(response)
        } else {
            Err(GuardianProtocolError::NonSuccessResponse)
        }
    }

    pub fn rejection(
        request: &AuthenticatedGuardianRequest,
        code: GuardianRejectionCode,
    ) -> Self {
        let payload = code.encode().to_vec();
        Self {
            header: GuardianResponseHeader::new(&request.header, code.status(), &payload),
            payload,
        }
    }
}

impl std::fmt::Debug for GuardianResponseEnvelope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianResponseEnvelope")
            .field("protocol_version", &self.header.protocol_version)
            .field("operation", &self.header.operation)
            .field("status", &self.header.status)
            .field("payload_len", &self.payload.len())
            .finish_non_exhaustive()
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

#[derive(Clone, PartialEq)]
pub struct GuardianSpawnPayload {
    command: CommandBuilder,
    size: PtySize,
}

impl std::fmt::Debug for GuardianSpawnPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianSpawnPayload")
            .field("command_args", &self.command.get_argv().len())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl GuardianSpawnPayload {
    pub fn new(command: CommandBuilder, size: PtySize) -> Result<Self, GuardianProtocolError> {
        let payload = Self { command, size };
        payload.validate()?;
        Ok(payload)
    }

    #[must_use]
    pub const fn command(&self) -> &CommandBuilder {
        &self.command
    }

    #[must_use]
    pub const fn size(&self) -> PtySize {
        self.size
    }

    pub fn into_parts(self) -> (CommandBuilder, PtySize) {
        (self.command, self.size)
    }

    pub fn encode(&self) -> Result<Vec<u8>, GuardianProtocolError> {
        self.validate()?;
        let command_limit = GUARDIAN_MAX_PAYLOAD_BYTES
            .checked_sub(SPAWN_PAYLOAD_FIXED_BYTES)
            .ok_or(GuardianProtocolError::PayloadTooLarge)?;
        let mut command = GuardianBoundedPayloadBuffer::new(command_limit);
        if serde_json::to_writer(&mut command, &self.command).is_err() {
            return Err(if command.exceeded {
                GuardianProtocolError::PayloadTooLarge
            } else {
                GuardianProtocolError::InvalidOperationPayload
            });
        }
        let command = command.into_inner();
        let total = SPAWN_PAYLOAD_FIXED_BYTES
            .checked_add(command.len())
            .ok_or(GuardianProtocolError::PayloadTooLarge)?;
        if total > GUARDIAN_MAX_PAYLOAD_BYTES {
            return Err(GuardianProtocolError::PayloadTooLarge);
        }
        let mut payload = Vec::with_capacity(total);
        payload.extend_from_slice(&SPAWN_PAYLOAD_MAGIC);
        encode_pty_size(&mut payload, self.size);
        payload.extend_from_slice(
            &u32::try_from(command.len())
                .map_err(|_| GuardianProtocolError::PayloadTooLarge)?
                .to_be_bytes(),
        );
        payload.extend_from_slice(command.as_slice());
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() < SPAWN_PAYLOAD_FIXED_BYTES
            || payload.len() > GUARDIAN_MAX_PAYLOAD_BYTES
            || payload.get(..4) != Some(SPAWN_PAYLOAD_MAGIC.as_slice())
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let size = decode_pty_size(
            payload
                .get(4..12)
                .ok_or(GuardianProtocolError::InvalidOperationPayload)?,
        )?;
        let command_len = usize::try_from(read_u32(payload, 12)?)
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let expected = SPAWN_PAYLOAD_FIXED_BYTES
            .checked_add(command_len)
            .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
        if payload.len() != expected {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let command_bytes = payload
            .get(SPAWN_PAYLOAD_FIXED_BYTES..)
            .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
        let command: CommandBuilder = serde_json::from_slice(command_bytes)
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let command_limit = GUARDIAN_MAX_PAYLOAD_BYTES
            .checked_sub(SPAWN_PAYLOAD_FIXED_BYTES)
            .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
        let mut canonical = GuardianBoundedPayloadBuffer::new(command_limit);
        serde_json::to_writer(&mut canonical, &command)
            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
        let canonical = canonical.into_inner();
        if canonical.as_slice() != command_bytes {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Self::new(command, size)
    }

    fn validate(&self) -> Result<(), GuardianProtocolError> {
        if self
            .command
            .get_argv()
            .first()
            .is_none_or(|program| program.is_empty())
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianResizePayload(PtySize);

impl GuardianResizePayload {
    #[must_use]
    pub const fn new(size: PtySize) -> Self {
        Self(size)
    }

    #[must_use]
    pub const fn size(self) -> PtySize {
        self.0
    }

    #[must_use]
    pub fn encode(self) -> [u8; RESIZE_PAYLOAD_BYTES] {
        let mut payload = Vec::with_capacity(RESIZE_PAYLOAD_BYTES);
        payload.extend_from_slice(&RESIZE_PAYLOAD_MAGIC);
        encode_pty_size(&mut payload, self.0);
        let mut encoded = [0_u8; RESIZE_PAYLOAD_BYTES];
        encoded.copy_from_slice(&payload);
        encoded
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != RESIZE_PAYLOAD_BYTES
            || payload.get(..4) != Some(RESIZE_PAYLOAD_MAGIC.as_slice())
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(Self::new(decode_pty_size(&payload[4..])?))
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianSignal {
    Terminate = 1,
}

impl GuardianSignal {
    #[must_use]
    pub fn encode(self) -> [u8; SIGNAL_PAYLOAD_BYTES] {
        let mut payload = [0_u8; SIGNAL_PAYLOAD_BYTES];
        payload[..4].copy_from_slice(&SIGNAL_PAYLOAD_MAGIC);
        payload[4] = self as u8;
        payload
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != SIGNAL_PAYLOAD_BYTES
            || payload.get(..4) != Some(SIGNAL_PAYLOAD_MAGIC.as_slice())
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        match payload[4] {
            1 => Ok(Self::Terminate),
            _ => Err(GuardianProtocolError::InvalidOperationPayload),
        }
    }
}

/// Opaque identity of one immutable checkpoint artifact.
///
/// The bytes may be content-derived, so diagnostics deliberately expose only
/// the type name. The all-zero value is reserved as an absent identity.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct GuardianCheckpointIdentityDigest([u8; 32]);

impl GuardianCheckpointIdentityDigest {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, GuardianProtocolError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(GuardianProtocolError::InvalidCheckpointIntent);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for GuardianCheckpointIdentityDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GuardianCheckpointIdentityDigest([REDACTED])")
    }
}

/// Opaque identity of the exact durable output boundary included by a
/// checkpoint. It is a separate type so callers cannot accidentally swap the
/// artifact and output-boundary digests.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct GuardianOutputBoundaryIdentityDigest([u8; 32]);

impl GuardianOutputBoundaryIdentityDigest {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, GuardianProtocolError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(GuardianProtocolError::InvalidCheckpointIntent);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for GuardianOutputBoundaryIdentityDigest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GuardianOutputBoundaryIdentityDigest([REDACTED])")
    }
}

/// Versioned, fixed-width checkpoint publication intent.
///
/// This payload binds the request header's pane, mux incarnation, lease
/// generation, mutation sequence, request nonce, and effect nonce to both the
/// checkpoint artifact and the exact durable output boundary it covers.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct GuardianCheckpointIntent {
    checkpoint_identity: GuardianCheckpointIdentityDigest,
    output_boundary_identity: GuardianOutputBoundaryIdentityDigest,
}

impl std::fmt::Debug for GuardianCheckpointIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointIntent")
            .field("version", &GUARDIAN_CHECKPOINT_INTENT_VERSION)
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointIntent {
    #[must_use]
    pub const fn new(
        checkpoint_identity: GuardianCheckpointIdentityDigest,
        output_boundary_identity: GuardianOutputBoundaryIdentityDigest,
    ) -> Self {
        Self {
            checkpoint_identity,
            output_boundary_identity,
        }
    }

    #[must_use]
    pub const fn checkpoint_identity(self) -> GuardianCheckpointIdentityDigest {
        self.checkpoint_identity
    }

    #[must_use]
    pub const fn output_boundary_identity(self) -> GuardianOutputBoundaryIdentityDigest {
        self.output_boundary_identity
    }

    #[must_use]
    pub fn encode(self) -> [u8; GUARDIAN_CHECKPOINT_INTENT_BYTES] {
        let mut payload = [0_u8; GUARDIAN_CHECKPOINT_INTENT_BYTES];
        payload[..4].copy_from_slice(&CHECKPOINT_INTENT_PAYLOAD_MAGIC);
        payload[4..6].copy_from_slice(&GUARDIAN_CHECKPOINT_INTENT_VERSION.to_be_bytes());
        payload[8..40].copy_from_slice(&self.checkpoint_identity.0);
        payload[40..72].copy_from_slice(&self.output_boundary_identity.0);
        payload
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != GUARDIAN_CHECKPOINT_INTENT_BYTES
            || payload.get(..4) != Some(CHECKPOINT_INTENT_PAYLOAD_MAGIC.as_slice())
            || read_u16(payload, 4)? != GUARDIAN_CHECKPOINT_INTENT_VERSION
            || payload[6] != 0
            || payload[7] != 0
        {
            return Err(GuardianProtocolError::InvalidCheckpointIntent);
        }
        let mut checkpoint_identity = [0_u8; 32];
        checkpoint_identity.copy_from_slice(&payload[8..40]);
        let mut output_boundary_identity = [0_u8; 32];
        output_boundary_identity.copy_from_slice(&payload[40..72]);
        Ok(Self::new(
            GuardianCheckpointIdentityDigest::from_bytes(checkpoint_identity)?,
            GuardianOutputBoundaryIdentityDigest::from_bytes(output_boundary_identity)?,
        ))
    }
}

/// Retained durable-publication disposition for a checkpoint effect.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCheckpointDisposition {
    Committed = 1,
    /// Publication may have become durable, so the operation is pinned and
    /// must not be retried until the exact identity is reconciled.
    OutcomeIndeterminate = 2,
}

impl GuardianCheckpointDisposition {
    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            1 => Ok(Self::Committed),
            2 => Ok(Self::OutcomeIndeterminate),
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
    }
}

/// Exact authenticated identity handed to the checkpoint publisher and later
/// required for reconciliation. Digest fields remain opaque in diagnostics.
#[derive(Clone, Copy, Eq, PartialEq)]
struct GuardianCheckpointEffectIdentity {
    pane_id: Uuid,
    mux_incarnation: Uuid,
    request_id: Uuid,
    generation: u64,
    sequence: u64,
    effect_id: Uuid,
    intent: GuardianCheckpointIntent,
}

impl std::fmt::Debug for GuardianCheckpointEffectIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointEffectIdentity")
            .field("pane_id", &self.pane_id)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("request_id", &self.request_id)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("effect_id", &self.effect_id)
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointEffectIdentity {
    fn from_authenticated_request(
        request: &AuthenticatedGuardianRequest,
    ) -> Result<Self, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.operation != GuardianOperation::Checkpoint {
            return Err(GuardianProtocolError::CheckpointIdentityMismatch);
        }
        Ok(Self {
            pane_id: request
                .header
                .pane_id
                .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?,
            mux_incarnation: request.header.mux_incarnation,
            request_id: request.header.request_id,
            generation: request.header.lease_generation,
            sequence: request.header.lease_sequence,
            effect_id: request
                .header
                .effect_id
                .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?,
            intent: GuardianCheckpointIntent::decode(&request.payload)?,
        })
    }

    #[must_use]
    #[cfg(test)]
    const fn pane_id(self) -> Uuid {
        self.pane_id
    }

    #[must_use]
    #[cfg(test)]
    const fn mux_incarnation(self) -> Uuid {
        self.mux_incarnation
    }

    #[must_use]
    #[cfg(test)]
    const fn request_id(self) -> Uuid {
        self.request_id
    }

    #[must_use]
    #[cfg(test)]
    const fn generation(self) -> u64 {
        self.generation
    }

    #[must_use]
    #[cfg(test)]
    const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    #[cfg(test)]
    const fn effect_id(self) -> Uuid {
        self.effect_id
    }

    #[must_use]
    #[cfg(test)]
    const fn intent(self) -> GuardianCheckpointIntent {
        self.intent
    }
}

/// Nonconstructible pending-publication authority passed only by the protocol
/// state machine after every fence and capacity check succeeds.
struct GuardianCheckpointPending {
    identity: GuardianCheckpointEffectIdentity,
}

impl std::fmt::Debug for GuardianCheckpointPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointPending")
            .field("identity", &self.identity)
            .finish()
    }
}

impl GuardianCheckpointPending {
    #[must_use]
    #[cfg(test)]
    const fn identity(&self) -> GuardianCheckpointEffectIdentity {
        self.identity
    }
}

/// Authenticated checkpoint receipt. `OutcomeIndeterminate` is terminal for
/// blind retry and is encoded under a distinct response status.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointReceipt {
    pane_id: Uuid,
    generation: u64,
    sequence: u64,
    effect_id: Uuid,
    intent: GuardianCheckpointIntent,
    disposition: GuardianCheckpointDisposition,
}

impl std::fmt::Debug for GuardianCheckpointReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointReceipt")
            .field("pane_id", &self.pane_id)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("effect_id", &self.effect_id)
            .field("disposition", &self.disposition)
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointReceipt {
    fn from_identity(
        identity: GuardianCheckpointEffectIdentity,
        disposition: GuardianCheckpointDisposition,
    ) -> Self {
        Self {
            pane_id: identity.pane_id,
            generation: identity.generation,
            sequence: identity.sequence,
            effect_id: identity.effect_id,
            intent: identity.intent,
            disposition,
        }
    }

    #[must_use]
    pub const fn pane_id(self) -> Uuid {
        self.pane_id
    }

    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn effect_id(self) -> Uuid {
        self.effect_id
    }

    #[must_use]
    pub const fn intent(self) -> GuardianCheckpointIntent {
        self.intent
    }

    #[must_use]
    pub const fn disposition(self) -> GuardianCheckpointDisposition {
        self.disposition
    }

    fn encode(self) -> [u8; GUARDIAN_CHECKPOINT_RECEIPT_BYTES] {
        let mut payload = [0_u8; GUARDIAN_CHECKPOINT_RECEIPT_BYTES];
        payload[..4].copy_from_slice(&CHECKPOINT_RECEIPT_PAYLOAD_MAGIC);
        payload[4..6].copy_from_slice(&GUARDIAN_CHECKPOINT_INTENT_VERSION.to_be_bytes());
        payload[6] = self.disposition as u8;
        payload[8..24].copy_from_slice(self.pane_id.as_bytes());
        payload[24..32].copy_from_slice(&self.generation.to_be_bytes());
        payload[32..40].copy_from_slice(&self.sequence.to_be_bytes());
        payload[40..56].copy_from_slice(self.effect_id.as_bytes());
        payload[56..88].copy_from_slice(&self.intent.checkpoint_identity.0);
        payload[88..120].copy_from_slice(&self.intent.output_boundary_identity.0);
        payload
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != GUARDIAN_CHECKPOINT_RECEIPT_BYTES
            || payload.get(..4) != Some(CHECKPOINT_RECEIPT_PAYLOAD_MAGIC.as_slice())
            || read_u16(payload, 4)? != GUARDIAN_CHECKPOINT_INTENT_VERSION
            || payload[7] != 0
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let mut checkpoint_identity = [0_u8; 32];
        checkpoint_identity.copy_from_slice(&payload[56..88]);
        let mut output_boundary_identity = [0_u8; 32];
        output_boundary_identity.copy_from_slice(&payload[88..120]);
        let receipt = Self {
            pane_id: read_required_uuid(payload, 8)?,
            generation: read_u64(payload, 24)?,
            sequence: read_u64(payload, 32)?,
            effect_id: read_required_uuid(payload, 40)?,
            intent: GuardianCheckpointIntent::new(
                GuardianCheckpointIdentityDigest::from_bytes(checkpoint_identity)
                    .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
                GuardianOutputBoundaryIdentityDigest::from_bytes(output_boundary_identity)
                    .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
            ),
            disposition: GuardianCheckpointDisposition::from_wire(payload[6])?,
        };
        if receipt.generation == 0 || receipt.sequence == 0 {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        Ok(receipt)
    }

    fn matches_identity(self, identity: GuardianCheckpointEffectIdentity) -> bool {
        self.pane_id == identity.pane_id
            && self.generation == identity.generation
            && self.sequence == identity.sequence
            && self.effect_id == identity.effect_id
            && self.intent == identity.intent
    }
}

/// Scope of an immutable checkpoint upload. A live pane upload is fenced by
/// its exact lease generation. A genesis upload is instead bound to the spawn
/// effect that must adopt it before a child can emit output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCheckpointScopeV1 {
    Pane { pane_id: Uuid, generation: u64 },
    Genesis { spawn_effect_id: Uuid },
}

impl GuardianCheckpointScopeV1 {
    fn validate(self) -> Result<(), GuardianProtocolError> {
        match self {
            Self::Pane {
                pane_id,
                generation,
            } if !pane_id.is_nil() && generation > 0 => Ok(()),
            Self::Genesis { spawn_effect_id } if !spawn_effect_id.is_nil() => Ok(()),
            _ => Err(GuardianProtocolError::InvalidOperationPayload),
        }
    }

    fn encode_into(self, payload: &mut Vec<u8>) {
        match self {
            Self::Pane {
                pane_id,
                generation,
            } => {
                payload.push(1);
                payload.extend_from_slice(&[0; 7]);
                push_uuid(payload, pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
            }
            Self::Genesis { spawn_effect_id } => {
                payload.push(2);
                payload.extend_from_slice(&[0; 7]);
                push_uuid(payload, spawn_effect_id);
                payload.extend_from_slice(&0_u64.to_be_bytes());
            }
        }
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != CHECKPOINT_STAGE_SCOPE_BYTES
            || payload.get(1..8) != Some([0; 7].as_slice())
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let identity = read_required_uuid(payload, 8)?;
        let generation = read_u64(payload, 24)?;
        let scope = match payload[0] {
            1 if generation > 0 => Self::Pane {
                pane_id: identity,
                generation,
            },
            2 if generation == 0 => Self::Genesis {
                spawn_effect_id: identity,
            },
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        };
        scope.validate()?;
        Ok(scope)
    }

    fn matches_header(self, header: &GuardianRequestHeader) -> bool {
        match self {
            Self::Pane {
                pane_id,
                generation,
            } => {
                header.pane_id == Some(pane_id)
                    && header.effect_id.is_none()
                    && header.lease_generation == generation
                    && header.lease_sequence == 0
            }
            Self::Genesis { spawn_effect_id } => {
                header.pane_id.is_none()
                    && header.effect_id == Some(spawn_effect_id)
                    && header.lease_generation == 0
                    && header.lease_sequence == 0
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCheckpointStageKindV1 {
    Begin,
    Chunk,
    Seal,
}

enum GuardianCheckpointStageBodyV1 {
    Begin,
    Chunk {
        index: u32,
        offset: u64,
        chunk_digest: [u8; 32],
        bytes: Zeroizing<Vec<u8>>,
    },
    Seal,
}

/// Canonical, self-describing upload operation. Every chunk repeats the
/// immutable upload descriptor, so an exact retry can be reconciled without
/// consulting unauthenticated or partially written state.
pub struct GuardianCheckpointStageRequestV1 {
    scope: GuardianCheckpointScopeV1,
    upload_id: Uuid,
    checkpoint_id: GuardianCheckpointIdentityDigest,
    boundary_id: GuardianOutputBoundaryIdentityDigest,
    total_bytes: u64,
    chunk_bytes: u32,
    total_chunks: u32,
    body: GuardianCheckpointStageBodyV1,
}

impl std::fmt::Debug for GuardianCheckpointStageRequestV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointStageRequestV1")
            .field("scope", &self.scope)
            .field("upload_id", &self.upload_id)
            .field("kind", &self.kind())
            .field("total_bytes", &self.total_bytes)
            .field("chunk_bytes", &self.chunk_bytes)
            .field("total_chunks", &self.total_chunks)
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointStageRequestV1 {
    pub fn begin(
        scope: GuardianCheckpointScopeV1,
        upload_id: Uuid,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        total_bytes: u64,
        chunk_bytes: u32,
    ) -> Result<Self, GuardianProtocolError> {
        Self::new(
            scope,
            upload_id,
            checkpoint_id,
            boundary_id,
            total_bytes,
            chunk_bytes,
            GuardianCheckpointStageBodyV1::Begin,
        )
    }

    pub fn chunk(
        scope: GuardianCheckpointScopeV1,
        upload_id: Uuid,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        total_bytes: u64,
        chunk_bytes: u32,
        index: u32,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianProtocolError> {
        let offset = u64::from(index)
            .checked_mul(u64::from(chunk_bytes))
            .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
        let chunk_digest = Sha256::digest(bytes.as_slice()).into();
        Self::new(
            scope,
            upload_id,
            checkpoint_id,
            boundary_id,
            total_bytes,
            chunk_bytes,
            GuardianCheckpointStageBodyV1::Chunk {
                index,
                offset,
                chunk_digest,
                bytes,
            },
        )
    }

    pub fn seal(
        scope: GuardianCheckpointScopeV1,
        upload_id: Uuid,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        total_bytes: u64,
        chunk_bytes: u32,
    ) -> Result<Self, GuardianProtocolError> {
        Self::new(
            scope,
            upload_id,
            checkpoint_id,
            boundary_id,
            total_bytes,
            chunk_bytes,
            GuardianCheckpointStageBodyV1::Seal,
        )
    }

    fn new(
        scope: GuardianCheckpointScopeV1,
        upload_id: Uuid,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        total_bytes: u64,
        chunk_bytes: u32,
        body: GuardianCheckpointStageBodyV1,
    ) -> Result<Self, GuardianProtocolError> {
        let total_chunks = checkpoint_total_chunks(total_bytes, chunk_bytes)?;
        let request = Self {
            scope,
            upload_id,
            checkpoint_id,
            boundary_id,
            total_bytes,
            chunk_bytes,
            total_chunks,
            body,
        };
        request.validate()?;
        Ok(request)
    }

    #[must_use]
    pub const fn scope(&self) -> GuardianCheckpointScopeV1 {
        self.scope
    }

    #[must_use]
    pub const fn upload_id(&self) -> Uuid {
        self.upload_id
    }

    #[must_use]
    pub const fn checkpoint_id(&self) -> GuardianCheckpointIdentityDigest {
        self.checkpoint_id
    }

    #[must_use]
    pub const fn boundary_id(&self) -> GuardianOutputBoundaryIdentityDigest {
        self.boundary_id
    }

    #[must_use]
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    pub const fn chunk_bytes(&self) -> u32 {
        self.chunk_bytes
    }

    #[must_use]
    pub const fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    #[must_use]
    pub const fn kind(&self) -> GuardianCheckpointStageKindV1 {
        match self.body {
            GuardianCheckpointStageBodyV1::Begin => GuardianCheckpointStageKindV1::Begin,
            GuardianCheckpointStageBodyV1::Chunk { .. } => GuardianCheckpointStageKindV1::Chunk,
            GuardianCheckpointStageBodyV1::Seal => GuardianCheckpointStageKindV1::Seal,
        }
    }

    #[must_use]
    pub const fn chunk_position(&self) -> Option<(u32, u64, [u8; 32])> {
        match &self.body {
            GuardianCheckpointStageBodyV1::Chunk {
                index,
                offset,
                chunk_digest,
                ..
            } => Some((*index, *offset, *chunk_digest)),
            GuardianCheckpointStageBodyV1::Begin | GuardianCheckpointStageBodyV1::Seal => None,
        }
    }

    pub fn into_chunk_bytes(self) -> Result<Zeroizing<Vec<u8>>, GuardianProtocolError> {
        match self.body {
            GuardianCheckpointStageBodyV1::Chunk { bytes, .. } => Ok(bytes),
            GuardianCheckpointStageBodyV1::Begin | GuardianCheckpointStageBodyV1::Seal => {
                Err(GuardianProtocolError::InvalidOperationPayload)
            }
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, GuardianProtocolError> {
        self.validate()?;
        let extra = match &self.body {
            GuardianCheckpointStageBodyV1::Chunk { bytes, .. } => bytes.len(),
            GuardianCheckpointStageBodyV1::Begin | GuardianCheckpointStageBodyV1::Seal => 0,
        };
        let fixed = if matches!(&self.body, GuardianCheckpointStageBodyV1::Chunk { .. }) {
            CHECKPOINT_STAGE_CHUNK_FIXED_BYTES
        } else {
            CHECKPOINT_STAGE_COMMON_BYTES
        };
        let capacity = fixed
            .checked_add(extra)
            .ok_or(GuardianProtocolError::PayloadTooLarge)?;
        if capacity > GUARDIAN_MAX_PAYLOAD_BYTES {
            return Err(GuardianProtocolError::PayloadTooLarge);
        }
        let mut payload = Vec::with_capacity(capacity);
        payload.extend_from_slice(&CHECKPOINT_STAGE_PAYLOAD_MAGIC);
        payload.extend_from_slice(&CHECKPOINT_STAGE_WIRE_VERSION.to_be_bytes());
        payload.push(match self.body {
            GuardianCheckpointStageBodyV1::Begin => 1,
            GuardianCheckpointStageBodyV1::Chunk { .. } => 2,
            GuardianCheckpointStageBodyV1::Seal => 3,
        });
        payload.push(0);
        self.scope.encode_into(&mut payload);
        push_uuid(&mut payload, self.upload_id);
        payload.extend_from_slice(&self.checkpoint_id.0);
        payload.extend_from_slice(&self.boundary_id.0);
        payload.extend_from_slice(&self.total_bytes.to_be_bytes());
        payload.extend_from_slice(&self.chunk_bytes.to_be_bytes());
        payload.extend_from_slice(&self.total_chunks.to_be_bytes());
        if let GuardianCheckpointStageBodyV1::Chunk {
            index,
            offset,
            chunk_digest,
            bytes,
        } = &self.body
        {
            payload.extend_from_slice(&index.to_be_bytes());
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(chunk_digest);
            payload.extend_from_slice(
                &u32::try_from(bytes.len())
                    .map_err(|_| GuardianProtocolError::PayloadTooLarge)?
                    .to_be_bytes(),
            );
            payload.extend_from_slice(bytes.as_slice());
        }
        if payload.len() != capacity {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-stage-encoded-size",
            ));
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() < CHECKPOINT_STAGE_COMMON_BYTES
            || payload.len() > GUARDIAN_MAX_PAYLOAD_BYTES
            || payload.get(..4) != Some(CHECKPOINT_STAGE_PAYLOAD_MAGIC.as_slice())
            || read_u16(payload, 4)? != CHECKPOINT_STAGE_WIRE_VERSION
            || payload[7] != 0
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let scope = GuardianCheckpointScopeV1::decode(
            payload
                .get(8..40)
                .ok_or(GuardianProtocolError::InvalidOperationPayload)?,
        )?;
        let upload_id = read_required_uuid(payload, 40)?;
        let mut checkpoint_id = [0; 32];
        checkpoint_id.copy_from_slice(&payload[56..88]);
        let mut boundary_id = [0; 32];
        boundary_id.copy_from_slice(&payload[88..120]);
        let total_bytes = read_u64(payload, 120)?;
        let chunk_bytes = read_u32(payload, 128)?;
        let total_chunks = read_u32(payload, 132)?;
        if checkpoint_total_chunks(total_bytes, chunk_bytes)? != total_chunks {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let body = match payload[6] {
            1 if payload.len() == CHECKPOINT_STAGE_COMMON_BYTES => {
                GuardianCheckpointStageBodyV1::Begin
            }
            2 if payload.len() >= CHECKPOINT_STAGE_CHUNK_FIXED_BYTES => {
                let index = read_u32(payload, 136)?;
                let offset = read_u64(payload, 140)?;
                let mut chunk_digest = [0; 32];
                chunk_digest.copy_from_slice(&payload[148..180]);
                let encoded_len = usize::try_from(read_u32(payload, 180)?)
                    .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
                let expected = CHECKPOINT_STAGE_CHUNK_FIXED_BYTES
                    .checked_add(encoded_len)
                    .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
                if payload.len() != expected {
                    return Err(GuardianProtocolError::InvalidOperationPayload);
                }
                let bytes = Zeroizing::new(payload[CHECKPOINT_STAGE_CHUNK_FIXED_BYTES..].to_vec());
                GuardianCheckpointStageBodyV1::Chunk {
                    index,
                    offset,
                    chunk_digest,
                    bytes,
                }
            }
            3 if payload.len() == CHECKPOINT_STAGE_COMMON_BYTES => {
                GuardianCheckpointStageBodyV1::Seal
            }
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        };
        let request = Self {
            scope,
            upload_id,
            checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes(checkpoint_id)
                .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?,
            boundary_id: GuardianOutputBoundaryIdentityDigest::from_bytes(boundary_id)
                .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?,
            total_bytes,
            chunk_bytes,
            total_chunks,
            body,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), GuardianProtocolError> {
        self.scope.validate()?;
        require_nonzero(self.upload_id, "checkpoint upload")?;
        if checkpoint_total_chunks(self.total_bytes, self.chunk_bytes)? != self.total_chunks {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        if let GuardianCheckpointStageBodyV1::Chunk {
            index,
            offset,
            chunk_digest,
            bytes,
        } = &self.body
        {
            if *index >= self.total_chunks
                || *offset != u64::from(*index) * u64::from(self.chunk_bytes)
                || bytes.is_empty()
                || bytes.len()
                    > usize::try_from(GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES)
                        .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?
            {
                return Err(GuardianProtocolError::InvalidOperationPayload);
            }
            let remaining = self
                .total_bytes
                .checked_sub(*offset)
                .ok_or(GuardianProtocolError::InvalidOperationPayload)?;
            let expected = remaining.min(u64::from(self.chunk_bytes));
            if u64::try_from(bytes.len()).ok() != Some(expected)
                || <[u8; 32]>::from(Sha256::digest(bytes.as_slice())) != *chunk_digest
            {
                return Err(GuardianProtocolError::InvalidOperationPayload);
            }
        }
        Ok(())
    }

    fn validate_header(&self, header: &GuardianRequestHeader) -> Result<(), GuardianProtocolError> {
        if self.scope.matches_header(header) {
            Ok(())
        } else {
            Err(GuardianProtocolError::InvalidOperationScope {
                operation: GuardianOperation::CheckpointStage,
            })
        }
    }
}

fn checkpoint_total_chunks(total_bytes: u64, chunk_bytes: u32) -> Result<u32, GuardianProtocolError> {
    if total_bytes == 0
        || total_bytes > GUARDIAN_MAX_CHECKPOINT_BYTES
        || chunk_bytes == 0
        || chunk_bytes > GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES
    {
        return Err(GuardianProtocolError::InvalidOperationPayload);
    }
    let chunks = total_bytes
        .checked_add(u64::from(chunk_bytes) - 1)
        .ok_or(GuardianProtocolError::InvalidOperationPayload)?
        / u64::from(chunk_bytes);
    let chunks = u32::try_from(chunks).map_err(|_| GuardianProtocolError::InvalidOperationPayload)?;
    if chunks == 0 || chunks > GUARDIAN_MAX_CHECKPOINT_CHUNKS {
        return Err(GuardianProtocolError::InvalidOperationPayload);
    }
    Ok(chunks)
}

/// Metadata-only result of an idempotent checkpoint staging request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCheckpointStageReplyV1 {
    Ready {
        upload_id: Uuid,
        next_index: u32,
        committed_bytes: u64,
    },
    Progress {
        upload_id: Uuid,
        next_index: u32,
        committed_bytes: u64,
    },
    Sealed {
        upload_id: Uuid,
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        total_bytes: u64,
    },
}

impl GuardianCheckpointStageReplyV1 {
    fn encode(self) -> Result<[u8; CHECKPOINT_STAGE_REPLY_BYTES], GuardianProtocolError> {
        self.validate()?;
        let mut payload = [0; CHECKPOINT_STAGE_REPLY_BYTES];
        payload[..4].copy_from_slice(&CHECKPOINT_STAGE_REPLY_MAGIC);
        payload[4..6].copy_from_slice(&CHECKPOINT_STAGE_WIRE_VERSION.to_be_bytes());
        match self {
            Self::Ready {
                upload_id,
                next_index,
                committed_bytes,
            }
            | Self::Progress {
                upload_id,
                next_index,
                committed_bytes,
            } => {
                payload[6] = if matches!(self, Self::Ready { .. }) { 1 } else { 2 };
                payload[8..24].copy_from_slice(upload_id.as_bytes());
                payload[24..28].copy_from_slice(&next_index.to_be_bytes());
                payload[28..36].copy_from_slice(&committed_bytes.to_be_bytes());
            }
            Self::Sealed {
                upload_id,
                checkpoint_id,
                boundary_id,
                total_bytes,
            } => {
                payload[6] = 3;
                payload[8..24].copy_from_slice(upload_id.as_bytes());
                payload[28..36].copy_from_slice(&total_bytes.to_be_bytes());
                payload[36..68].copy_from_slice(&checkpoint_id.0);
                payload[68..100].copy_from_slice(&boundary_id.0);
            }
        }
        Ok(payload)
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != CHECKPOINT_STAGE_REPLY_BYTES
            || payload.get(..4) != Some(CHECKPOINT_STAGE_REPLY_MAGIC.as_slice())
            || read_u16(payload, 4)? != CHECKPOINT_STAGE_WIRE_VERSION
            || payload[7] != 0
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let upload_id = read_required_uuid(payload, 8)?;
        let next_index = read_u32(payload, 24)?;
        let committed_bytes = read_u64(payload, 28)?;
        let reply = match payload[6] {
            1 | 2 if payload[36..].iter().all(|byte| *byte == 0) => {
                if payload[6] == 1 {
                    Self::Ready {
                        upload_id,
                        next_index,
                        committed_bytes,
                    }
                } else {
                    Self::Progress {
                        upload_id,
                        next_index,
                        committed_bytes,
                    }
                }
            }
            3 if next_index == 0 => {
                let mut checkpoint_id = [0; 32];
                checkpoint_id.copy_from_slice(&payload[36..68]);
                let mut boundary_id = [0; 32];
                boundary_id.copy_from_slice(&payload[68..100]);
                Self::Sealed {
                    upload_id,
                    checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes(checkpoint_id)
                        .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
                    boundary_id: GuardianOutputBoundaryIdentityDigest::from_bytes(boundary_id)
                        .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
                    total_bytes: committed_bytes,
                }
            }
            _ => return Err(GuardianProtocolError::InvalidReplyPayload),
        };
        reply.validate()?;
        Ok(reply)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        let valid = match self {
            Self::Ready {
                upload_id,
                next_index,
                committed_bytes,
            }
            | Self::Progress {
                upload_id,
                next_index,
                committed_bytes,
            } => {
                !upload_id.is_nil()
                    && next_index <= GUARDIAN_MAX_CHECKPOINT_CHUNKS
                    && committed_bytes <= GUARDIAN_MAX_CHECKPOINT_BYTES
            }
            Self::Sealed {
                upload_id,
                total_bytes,
                ..
            } => {
                !upload_id.is_nil()
                    && total_bytes > 0
                    && total_bytes <= GUARDIAN_MAX_CHECKPOINT_BYTES
            }
        };
        if valid {
            Ok(())
        } else {
            Err(GuardianProtocolError::InvalidReplyPayload)
        }
    }

    #[must_use]
    pub const fn upload_id(self) -> Uuid {
        match self {
            Self::Ready { upload_id, .. }
            | Self::Progress { upload_id, .. }
            | Self::Sealed { upload_id, .. } => upload_id,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianReplayPhaseV1 {
    Checkpoint = 1,
    Output = 2,
}

impl GuardianReplayPhaseV1 {
    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            1 => Ok(Self::Checkpoint),
            2 => Ok(Self::Output),
            _ => Err(GuardianProtocolError::InvalidOperationPayload),
        }
    }
}

/// Authenticated continuation for one immutable replay snapshot. The digest
/// covers every cursor field and the negotiated page bounds, preventing a
/// cursor from being retargeted to a different snapshot or resource budget.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianReplayCursorV1 {
    snapshot_id: Uuid,
    snapshot_digest: [u8; 32],
    phase: GuardianReplayPhaseV1,
    page_index: u32,
    checkpoint_offset: u64,
    next_sequence: u64,
    previous_record_digest: [u8; 32],
    compaction_generation: u64,
    max_plaintext_bytes: u32,
    max_records: u16,
    cursor_digest: [u8; 32],
}

impl std::fmt::Debug for GuardianReplayCursorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianReplayCursorV1")
            .field("snapshot_id", &self.snapshot_id)
            .field("phase", &self.phase)
            .field("page_index", &self.page_index)
            .field("checkpoint_offset", &self.checkpoint_offset)
            .field("next_sequence", &self.next_sequence)
            .field("compaction_generation", &self.compaction_generation)
            .field("max_plaintext_bytes", &self.max_plaintext_bytes)
            .field("max_records", &self.max_records)
            .finish_non_exhaustive()
    }
}

impl GuardianReplayCursorV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot_id: Uuid,
        snapshot_digest: [u8; 32],
        phase: GuardianReplayPhaseV1,
        page_index: u32,
        checkpoint_offset: u64,
        next_sequence: u64,
        previous_record_digest: [u8; 32],
        compaction_generation: u64,
        max_plaintext_bytes: u32,
        max_records: u16,
    ) -> Result<Self, GuardianProtocolError> {
        let mut cursor = Self {
            snapshot_id,
            snapshot_digest,
            phase,
            page_index,
            checkpoint_offset,
            next_sequence,
            previous_record_digest,
            compaction_generation,
            max_plaintext_bytes,
            max_records,
            cursor_digest: [0; 32],
        };
        cursor.validate_fields()?;
        cursor.cursor_digest = cursor.compute_digest();
        Ok(cursor)
    }

    #[must_use]
    pub const fn snapshot_id(self) -> Uuid {
        self.snapshot_id
    }

    #[must_use]
    pub const fn snapshot_digest(self) -> [u8; 32] {
        self.snapshot_digest
    }

    #[must_use]
    pub const fn phase(self) -> GuardianReplayPhaseV1 {
        self.phase
    }

    #[must_use]
    pub const fn page_index(self) -> u32 {
        self.page_index
    }

    #[must_use]
    pub const fn checkpoint_offset(self) -> u64 {
        self.checkpoint_offset
    }

    #[must_use]
    pub const fn next_sequence(self) -> u64 {
        self.next_sequence
    }

    #[must_use]
    pub const fn previous_record_digest(self) -> [u8; 32] {
        self.previous_record_digest
    }

    #[must_use]
    pub const fn compaction_generation(self) -> u64 {
        self.compaction_generation
    }

    #[must_use]
    pub const fn max_plaintext_bytes(self) -> u32 {
        self.max_plaintext_bytes
    }

    #[must_use]
    pub const fn max_records(self) -> u16 {
        self.max_records
    }

    #[must_use]
    pub const fn digest(self) -> [u8; 32] {
        self.cursor_digest
    }

    fn encode(self) -> [u8; REPLAY_CURSOR_BYTES] {
        let mut payload = self.encode_prefix();
        payload[128..160].copy_from_slice(&self.cursor_digest);
        payload
    }

    fn encode_prefix(self) -> [u8; REPLAY_CURSOR_BYTES] {
        let mut payload = [0; REPLAY_CURSOR_BYTES];
        payload[..16].copy_from_slice(self.snapshot_id.as_bytes());
        payload[16..48].copy_from_slice(&self.snapshot_digest);
        payload[48] = self.phase as u8;
        payload[56..60].copy_from_slice(&self.page_index.to_be_bytes());
        payload[64..72].copy_from_slice(&self.checkpoint_offset.to_be_bytes());
        payload[72..80].copy_from_slice(&self.next_sequence.to_be_bytes());
        payload[80..112].copy_from_slice(&self.previous_record_digest);
        payload[112..120].copy_from_slice(&self.compaction_generation.to_be_bytes());
        payload[120..124].copy_from_slice(&self.max_plaintext_bytes.to_be_bytes());
        payload[124..126].copy_from_slice(&self.max_records.to_be_bytes());
        payload
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != REPLAY_CURSOR_BYTES
            || payload[49..56].iter().any(|byte| *byte != 0)
            || payload[60..64].iter().any(|byte| *byte != 0)
            || payload[126..128].iter().any(|byte| *byte != 0)
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let mut snapshot_digest = [0; 32];
        snapshot_digest.copy_from_slice(&payload[16..48]);
        let mut previous_record_digest = [0; 32];
        previous_record_digest.copy_from_slice(&payload[80..112]);
        let mut cursor_digest = [0; 32];
        cursor_digest.copy_from_slice(&payload[128..160]);
        let cursor = Self {
            snapshot_id: read_required_uuid(payload, 0)?,
            snapshot_digest,
            phase: GuardianReplayPhaseV1::from_wire(payload[48])?,
            page_index: read_u32(payload, 56)?,
            checkpoint_offset: read_u64(payload, 64)?,
            next_sequence: read_u64(payload, 72)?,
            previous_record_digest,
            compaction_generation: read_u64(payload, 112)?,
            max_plaintext_bytes: read_u32(payload, 120)?,
            max_records: read_u16(payload, 124)?,
            cursor_digest,
        };
        cursor.validate_fields()?;
        if cursor.compute_digest() != cursor.cursor_digest {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(cursor)
    }

    fn validate_fields(self) -> Result<(), GuardianProtocolError> {
        let previous_is_zero = digest_is_zero(self.previous_record_digest);
        if self.snapshot_id.is_nil()
            || digest_is_zero(self.snapshot_digest)
            || self.next_sequence == 0
            || self.max_plaintext_bytes == 0
            || self.max_plaintext_bytes > GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES
            || self.max_records == 0
            || self.max_records > GUARDIAN_MAX_REPLAY_RECORDS
            || (self.next_sequence == 1) != previous_is_zero
            || (self.phase == GuardianReplayPhaseV1::Output && self.checkpoint_offset != 0)
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(())
    }

    fn compute_digest(self) -> [u8; 32] {
        let prefix = self.encode_prefix();
        let mut hasher = Sha256::new();
        hasher.update(REPLAY_CURSOR_DIGEST_DOMAIN);
        hasher.update(&prefix[..128]);
        hasher.finalize().into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianReplaySelectorV1 {
    LatestCompatible,
    ExactCheckpoint {
        checkpoint_id: GuardianCheckpointIdentityDigest,
    },
    Resume {
        checkpoint_id: GuardianCheckpointIdentityDigest,
        next_sequence: u64,
        previous_record_digest: [u8; 32],
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianReplayRequestV1 {
    Open {
        selector: GuardianReplaySelectorV1,
        max_plaintext_bytes: u32,
        max_records: u16,
        wait_millis: u16,
    },
    Continue {
        cursor: GuardianReplayCursorV1,
    },
}

impl GuardianReplayRequestV1 {
    pub fn encode(self) -> Result<Vec<u8>, GuardianProtocolError> {
        self.validate()?;
        let mut payload = match self {
            Self::Open { .. } => Vec::with_capacity(REPLAY_OPEN_REQUEST_BYTES),
            Self::Continue { .. } => Vec::with_capacity(REPLAY_CONTINUE_REQUEST_BYTES),
        };
        payload.extend_from_slice(&REPLAY_REQUEST_PAYLOAD_MAGIC);
        payload.extend_from_slice(&REPLAY_WIRE_VERSION.to_be_bytes());
        match self {
            Self::Open {
                selector,
                max_plaintext_bytes,
                max_records,
                wait_millis,
            } => {
                payload.push(1);
                payload.push(match selector {
                    GuardianReplaySelectorV1::LatestCompatible => 1,
                    GuardianReplaySelectorV1::ExactCheckpoint { .. } => 2,
                    GuardianReplaySelectorV1::Resume { .. } => 3,
                });
                payload.extend_from_slice(&max_plaintext_bytes.to_be_bytes());
                payload.extend_from_slice(&max_records.to_be_bytes());
                payload.extend_from_slice(&wait_millis.to_be_bytes());
                match selector {
                    GuardianReplaySelectorV1::LatestCompatible => {
                        payload.extend_from_slice(&[0; 72]);
                    }
                    GuardianReplaySelectorV1::ExactCheckpoint { checkpoint_id } => {
                        payload.extend_from_slice(&checkpoint_id.0);
                        payload.extend_from_slice(&[0; 40]);
                    }
                    GuardianReplaySelectorV1::Resume {
                        checkpoint_id,
                        next_sequence,
                        previous_record_digest,
                    } => {
                        payload.extend_from_slice(&checkpoint_id.0);
                        payload.extend_from_slice(&next_sequence.to_be_bytes());
                        payload.extend_from_slice(&previous_record_digest);
                    }
                }
                payload.extend_from_slice(&[0; 4]);
            }
            Self::Continue { cursor } => {
                payload.push(2);
                payload.push(0);
                payload.extend_from_slice(&cursor.encode());
            }
        }
        let expected = match self {
            Self::Open { .. } => REPLAY_OPEN_REQUEST_BYTES,
            Self::Continue { .. } => REPLAY_CONTINUE_REQUEST_BYTES,
        };
        if payload.len() != expected {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "replay-request-encoded-size",
            ));
        }
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() < 8
            || payload.get(..4) != Some(REPLAY_REQUEST_PAYLOAD_MAGIC.as_slice())
            || read_u16(payload, 4)? != REPLAY_WIRE_VERSION
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let request = match payload[6] {
            1 if payload.len() == REPLAY_OPEN_REQUEST_BYTES => {
                if payload[88..92].iter().any(|byte| *byte != 0) {
                    return Err(GuardianProtocolError::InvalidOperationPayload);
                }
                let max_plaintext_bytes = read_u32(payload, 8)?;
                let max_records = read_u16(payload, 12)?;
                let wait_millis = read_u16(payload, 14)?;
                let mut checkpoint_id = [0; 32];
                checkpoint_id.copy_from_slice(&payload[16..48]);
                let next_sequence = read_u64(payload, 48)?;
                let mut previous_record_digest = [0; 32];
                previous_record_digest.copy_from_slice(&payload[56..88]);
                let selector = match payload[7] {
                    1 if digest_is_zero(checkpoint_id)
                        && next_sequence == 0
                        && digest_is_zero(previous_record_digest) =>
                    {
                        GuardianReplaySelectorV1::LatestCompatible
                    }
                    2 if next_sequence == 0 && digest_is_zero(previous_record_digest) => {
                        GuardianReplaySelectorV1::ExactCheckpoint {
                            checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes(
                                checkpoint_id,
                            )
                            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?,
                        }
                    }
                    3 => GuardianReplaySelectorV1::Resume {
                        checkpoint_id: GuardianCheckpointIdentityDigest::from_bytes(checkpoint_id)
                            .map_err(|_| GuardianProtocolError::InvalidOperationPayload)?,
                        next_sequence,
                        previous_record_digest,
                    },
                    _ => return Err(GuardianProtocolError::InvalidOperationPayload),
                };
                Self::Open {
                    selector,
                    max_plaintext_bytes,
                    max_records,
                    wait_millis,
                }
            }
            2 if payload.len() == REPLAY_CONTINUE_REQUEST_BYTES && payload[7] == 0 => {
                Self::Continue {
                    cursor: GuardianReplayCursorV1::decode(&payload[8..])?,
                }
            }
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        match self {
            Self::Open {
                selector,
                max_plaintext_bytes,
                max_records,
                wait_millis,
            } => {
                if max_plaintext_bytes == 0
                    || max_plaintext_bytes > GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES
                    || max_records == 0
                    || max_records > GUARDIAN_MAX_REPLAY_RECORDS
                    || wait_millis > GUARDIAN_MAX_REPLAY_WAIT_MILLIS
                {
                    return Err(GuardianProtocolError::InvalidOperationPayload);
                }
                if let GuardianReplaySelectorV1::Resume {
                    next_sequence,
                    previous_record_digest,
                    ..
                } = selector
                {
                    if next_sequence == 0
                        || (next_sequence == 1) != digest_is_zero(previous_record_digest)
                    {
                        return Err(GuardianProtocolError::InvalidOperationPayload);
                    }
                }
                Ok(())
            }
            Self::Continue { cursor } => {
                cursor.validate_fields()?;
                if cursor.compute_digest() != cursor.cursor_digest {
                    return Err(GuardianProtocolError::InvalidOperationPayload);
                }
                Ok(())
            }
        }
    }

    #[must_use]
    pub const fn incoming_cursor(self) -> Option<GuardianReplayCursorV1> {
        match self {
            Self::Open { .. } => None,
            Self::Continue { cursor } => Some(cursor),
        }
    }

    #[must_use]
    pub const fn limits(self) -> (u32, u16) {
        match self {
            Self::Open {
                max_plaintext_bytes,
                max_records,
                ..
            } => (max_plaintext_bytes, max_records),
            Self::Continue { cursor } => (cursor.max_plaintext_bytes, cursor.max_records),
        }
    }
}

/// Cumulative acknowledgement for the one page currently outstanding in a
/// replay snapshot. It cannot authorize compaction or retention advancement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianReplayAckV1 {
    snapshot_id: Uuid,
    snapshot_digest: [u8; 32],
    page_index: u32,
    page_digest: [u8; 32],
    next_cursor_digest: Option<[u8; 32]>,
    through_sequence: u64,
    through_record_digest: [u8; 32],
    release_if_complete: bool,
}

impl GuardianReplayAckV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot_id: Uuid,
        snapshot_digest: [u8; 32],
        page_index: u32,
        page_digest: [u8; 32],
        next_cursor_digest: Option<[u8; 32]>,
        through_sequence: u64,
        through_record_digest: [u8; 32],
        release_if_complete: bool,
    ) -> Result<Self, GuardianProtocolError> {
        let ack = Self {
            snapshot_id,
            snapshot_digest,
            page_index,
            page_digest,
            next_cursor_digest,
            through_sequence,
            through_record_digest,
            release_if_complete,
        };
        ack.validate(GuardianProtocolError::InvalidOperationPayload)?;
        Ok(ack)
    }

    #[must_use]
    pub const fn snapshot_id(self) -> Uuid {
        self.snapshot_id
    }

    #[must_use]
    pub const fn snapshot_digest(self) -> [u8; 32] {
        self.snapshot_digest
    }

    #[must_use]
    pub const fn page_index(self) -> u32 {
        self.page_index
    }

    #[must_use]
    pub const fn page_digest(self) -> [u8; 32] {
        self.page_digest
    }

    #[must_use]
    pub const fn next_cursor_digest(self) -> Option<[u8; 32]> {
        self.next_cursor_digest
    }

    #[must_use]
    pub const fn through_sequence(self) -> u64 {
        self.through_sequence
    }

    #[must_use]
    pub const fn through_record_digest(self) -> [u8; 32] {
        self.through_record_digest
    }

    #[must_use]
    pub const fn release_if_complete(self) -> bool {
        self.release_if_complete
    }

    pub fn encode(self) -> Result<[u8; REPLAY_ACK_BYTES], GuardianProtocolError> {
        self.validate(GuardianProtocolError::InvalidOperationPayload)?;
        let mut payload = [0; REPLAY_ACK_BYTES];
        payload[..4].copy_from_slice(&REPLAY_ACK_PAYLOAD_MAGIC);
        payload[4..6].copy_from_slice(&REPLAY_WIRE_VERSION.to_be_bytes());
        payload[6] = u8::from(self.release_if_complete);
        payload[8..24].copy_from_slice(self.snapshot_id.as_bytes());
        payload[24..56].copy_from_slice(&self.snapshot_digest);
        payload[56..60].copy_from_slice(&self.page_index.to_be_bytes());
        payload[60..92].copy_from_slice(&self.page_digest);
        if let Some(digest) = self.next_cursor_digest {
            payload[92] = 1;
            payload[96..128].copy_from_slice(&digest);
        }
        payload[128..136].copy_from_slice(&self.through_sequence.to_be_bytes());
        payload[136..168].copy_from_slice(&self.through_record_digest);
        Ok(payload)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != REPLAY_ACK_BYTES
            || payload.get(..4) != Some(REPLAY_ACK_PAYLOAD_MAGIC.as_slice())
            || read_u16(payload, 4)? != REPLAY_WIRE_VERSION
            || payload[6] > 1
            || payload[7] != 0
            || payload[93..96].iter().any(|byte| *byte != 0)
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let mut snapshot_digest = [0; 32];
        snapshot_digest.copy_from_slice(&payload[24..56]);
        let mut page_digest = [0; 32];
        page_digest.copy_from_slice(&payload[60..92]);
        let next_cursor_digest = match payload[92] {
            0 if payload[96..128].iter().all(|byte| *byte == 0) => None,
            1 => {
                let mut digest = [0; 32];
                digest.copy_from_slice(&payload[96..128]);
                Some(digest)
            }
            _ => return Err(GuardianProtocolError::InvalidOperationPayload),
        };
        let mut through_record_digest = [0; 32];
        through_record_digest.copy_from_slice(&payload[136..168]);
        let ack = Self {
            snapshot_id: read_required_uuid(payload, 8)?,
            snapshot_digest,
            page_index: read_u32(payload, 56)?,
            page_digest,
            next_cursor_digest,
            through_sequence: read_u64(payload, 128)?,
            through_record_digest,
            release_if_complete: payload[6] == 1,
        };
        ack.validate(GuardianProtocolError::InvalidOperationPayload)?;
        Ok(ack)
    }

    fn validate(self, error: GuardianProtocolError) -> Result<(), GuardianProtocolError> {
        let through_digest_is_zero = digest_is_zero(self.through_record_digest);
        if self.snapshot_id.is_nil()
            || digest_is_zero(self.snapshot_digest)
            || digest_is_zero(self.page_digest)
            || self
                .next_cursor_digest
                .is_some_and(digest_is_zero)
            || (self.through_sequence == 0) != through_digest_is_zero
            || (self.release_if_complete && self.next_cursor_digest.is_some())
        {
            Err(error)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianReplayAckReceiptV1 {
    snapshot_id: Uuid,
    page_index: u32,
    page_digest: [u8; 32],
    through_sequence: u64,
    through_record_digest: [u8; 32],
}

impl GuardianReplayAckReceiptV1 {
    pub fn from_ack(ack: GuardianReplayAckV1) -> Self {
        Self {
            snapshot_id: ack.snapshot_id,
            page_index: ack.page_index,
            page_digest: ack.page_digest,
            through_sequence: ack.through_sequence,
            through_record_digest: ack.through_record_digest,
        }
    }

    fn encode(self) -> Result<[u8; REPLAY_ACK_REPLY_BYTES], GuardianProtocolError> {
        self.validate()?;
        let mut payload = [0; REPLAY_ACK_REPLY_BYTES];
        payload[..4].copy_from_slice(&REPLAY_ACK_REPLY_MAGIC);
        payload[4..6].copy_from_slice(&REPLAY_WIRE_VERSION.to_be_bytes());
        payload[8..24].copy_from_slice(self.snapshot_id.as_bytes());
        payload[24..28].copy_from_slice(&self.page_index.to_be_bytes());
        payload[28..60].copy_from_slice(&self.page_digest);
        payload[60..68].copy_from_slice(&self.through_sequence.to_be_bytes());
        payload[68..100].copy_from_slice(&self.through_record_digest);
        Ok(payload)
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != REPLAY_ACK_REPLY_BYTES
            || payload.get(..4) != Some(REPLAY_ACK_REPLY_MAGIC.as_slice())
            || read_u16(payload, 4)? != REPLAY_WIRE_VERSION
            || payload[6..8].iter().any(|byte| *byte != 0)
            || payload[100..].iter().any(|byte| *byte != 0)
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let mut page_digest = [0; 32];
        page_digest.copy_from_slice(&payload[28..60]);
        let mut through_record_digest = [0; 32];
        through_record_digest.copy_from_slice(&payload[68..100]);
        let receipt = Self {
            snapshot_id: read_required_uuid(payload, 8)?,
            page_index: read_u32(payload, 24)?,
            page_digest,
            through_sequence: read_u64(payload, 60)?,
            through_record_digest,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        if self.snapshot_id.is_nil()
            || digest_is_zero(self.page_digest)
            || (self.through_sequence == 0) != digest_is_zero(self.through_record_digest)
        {
            Err(GuardianProtocolError::InvalidReplyPayload)
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub const fn snapshot_id(self) -> Uuid {
        self.snapshot_id
    }

    #[must_use]
    pub const fn page_index(self) -> u32 {
        self.page_index
    }

    #[must_use]
    pub const fn page_digest(self) -> [u8; 32] {
        self.page_digest
    }

    #[must_use]
    pub const fn through_sequence(self) -> u64 {
        self.through_sequence
    }

    #[must_use]
    pub const fn through_record_digest(self) -> [u8; 32] {
        self.through_record_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianCheckpointOutputBoundaryV1 {
    Genesis {
        spawn_effect_id: Uuid,
    },
    Record {
        segment_id: Uuid,
        sequence: u64,
        record_digest: [u8; 32],
        committed_log_bytes: u64,
        cumulative_plaintext_bytes: u64,
        parser_stream_bytes: u64,
    },
}

/// Metadata needed to select, bound, and assemble one canonical terminal
/// checkpoint. It deliberately contains no terminal plaintext.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointDescriptorV1 {
    checkpoint_id: GuardianCheckpointIdentityDigest,
    boundary_id: GuardianOutputBoundaryIdentityDigest,
    capture_generation: u64,
    replay_semantics_id: [u8; 32],
    rows: u32,
    cols: u32,
    total_bytes: u64,
    output_boundary: GuardianCheckpointOutputBoundaryV1,
}

impl std::fmt::Debug for GuardianCheckpointDescriptorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointDescriptorV1")
            .field("capture_generation", &self.capture_generation)
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("total_bytes", &self.total_bytes)
            .field("output_boundary", &self.output_boundary)
            .finish_non_exhaustive()
    }
}

impl GuardianCheckpointDescriptorV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        checkpoint_id: GuardianCheckpointIdentityDigest,
        boundary_id: GuardianOutputBoundaryIdentityDigest,
        capture_generation: u64,
        replay_semantics_id: [u8; 32],
        rows: u32,
        cols: u32,
        total_bytes: u64,
        output_boundary: GuardianCheckpointOutputBoundaryV1,
    ) -> Result<Self, GuardianProtocolError> {
        let descriptor = Self {
            checkpoint_id,
            boundary_id,
            capture_generation,
            replay_semantics_id,
            rows,
            cols,
            total_bytes,
            output_boundary,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    #[must_use]
    pub const fn checkpoint_id(self) -> GuardianCheckpointIdentityDigest {
        self.checkpoint_id
    }

    #[must_use]
    pub const fn boundary_id(self) -> GuardianOutputBoundaryIdentityDigest {
        self.boundary_id
    }

    #[must_use]
    pub const fn capture_generation(self) -> u64 {
        self.capture_generation
    }

    #[must_use]
    pub const fn replay_semantics_id(self) -> [u8; 32] {
        self.replay_semantics_id
    }

    #[must_use]
    pub const fn rows(self) -> u32 {
        self.rows
    }

    #[must_use]
    pub const fn cols(self) -> u32 {
        self.cols
    }

    #[must_use]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    #[must_use]
    pub const fn output_boundary(self) -> GuardianCheckpointOutputBoundaryV1 {
        self.output_boundary
    }

    #[must_use]
    pub const fn suffix_start(self) -> Option<(u64, [u8; 32])> {
        match self.output_boundary {
            GuardianCheckpointOutputBoundaryV1::Genesis { .. } => Some((1, [0; 32])),
            GuardianCheckpointOutputBoundaryV1::Record {
                sequence,
                record_digest,
                ..
            } => match sequence.checked_add(1) {
                Some(next) => Some((next, record_digest)),
                None => None,
            },
        }
    }

    fn encode(self) -> [u8; REPLAY_CHECKPOINT_DESCRIPTOR_BYTES] {
        let mut payload = [0; REPLAY_CHECKPOINT_DESCRIPTOR_BYTES];
        payload[..32].copy_from_slice(&self.checkpoint_id.0);
        payload[32..64].copy_from_slice(&self.boundary_id.0);
        payload[64..72].copy_from_slice(&self.capture_generation.to_be_bytes());
        payload[72..104].copy_from_slice(&self.replay_semantics_id);
        payload[104..108].copy_from_slice(&self.rows.to_be_bytes());
        payload[108..112].copy_from_slice(&self.cols.to_be_bytes());
        payload[112..120].copy_from_slice(&self.total_bytes.to_be_bytes());
        match self.output_boundary {
            GuardianCheckpointOutputBoundaryV1::Genesis { spawn_effect_id } => {
                payload[120] = 1;
                payload[128..144].copy_from_slice(spawn_effect_id.as_bytes());
            }
            GuardianCheckpointOutputBoundaryV1::Record {
                segment_id,
                sequence,
                record_digest,
                committed_log_bytes,
                cumulative_plaintext_bytes,
                parser_stream_bytes,
            } => {
                payload[120] = 2;
                payload[144..160].copy_from_slice(segment_id.as_bytes());
                payload[160..168].copy_from_slice(&sequence.to_be_bytes());
                payload[168..200].copy_from_slice(&record_digest);
                payload[200..208].copy_from_slice(&committed_log_bytes.to_be_bytes());
                payload[208..216].copy_from_slice(&cumulative_plaintext_bytes.to_be_bytes());
                payload[216..224].copy_from_slice(&parser_stream_bytes.to_be_bytes());
            }
        }
        payload
    }

    fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != REPLAY_CHECKPOINT_DESCRIPTOR_BYTES
            || payload[121..128].iter().any(|byte| *byte != 0)
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let mut checkpoint_id = [0; 32];
        checkpoint_id.copy_from_slice(&payload[..32]);
        let mut boundary_id = [0; 32];
        boundary_id.copy_from_slice(&payload[32..64]);
        let mut replay_semantics_id = [0; 32];
        replay_semantics_id.copy_from_slice(&payload[72..104]);
        let output_boundary = match payload[120] {
            1 if payload[144..].iter().all(|byte| *byte == 0) => {
                GuardianCheckpointOutputBoundaryV1::Genesis {
                    spawn_effect_id: read_required_uuid(payload, 128)?,
                }
            }
            2 if payload[128..144].iter().all(|byte| *byte == 0) => {
                let mut record_digest = [0; 32];
                record_digest.copy_from_slice(&payload[168..200]);
                GuardianCheckpointOutputBoundaryV1::Record {
                    segment_id: read_required_uuid(payload, 144)?,
                    sequence: read_u64(payload, 160)?,
                    record_digest,
                    committed_log_bytes: read_u64(payload, 200)?,
                    cumulative_plaintext_bytes: read_u64(payload, 208)?,
                    parser_stream_bytes: read_u64(payload, 216)?,
                }
            }
            _ => return Err(GuardianProtocolError::InvalidReplyPayload),
        };
        Self::new(
            GuardianCheckpointIdentityDigest::from_bytes(checkpoint_id)
                .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
            GuardianOutputBoundaryIdentityDigest::from_bytes(boundary_id)
                .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
            read_u64(payload, 64)?,
            replay_semantics_id,
            read_u32(payload, 104)?,
            read_u32(payload, 108)?,
            read_u64(payload, 112)?,
            output_boundary,
        )
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        let boundary_valid = match self.output_boundary {
            GuardianCheckpointOutputBoundaryV1::Genesis { spawn_effect_id } => {
                !spawn_effect_id.is_nil()
            }
            GuardianCheckpointOutputBoundaryV1::Record {
                segment_id,
                sequence,
                record_digest,
                committed_log_bytes,
                cumulative_plaintext_bytes,
                parser_stream_bytes,
            } => {
                !segment_id.is_nil()
                    && sequence > 0
                    && !digest_is_zero(record_digest)
                    && committed_log_bytes > 0
                    && cumulative_plaintext_bytes > 0
                    && parser_stream_bytes > 0
            }
        };
        if self.capture_generation == 0
            || digest_is_zero(self.replay_semantics_id)
            || self.rows == 0
            || self.cols == 0
            || self.total_bytes == 0
            || self.total_bytes > GUARDIAN_MAX_CHECKPOINT_BYTES
            || !boundary_valid
            || self.suffix_start().is_none()
        {
            Err(GuardianProtocolError::InvalidReplyPayload)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianReplayPredecessorV1 {
    segment_id: Uuid,
    last_sequence: u64,
    terminal_record_digest: [u8; 32],
    cumulative_plaintext_bytes: u64,
    committed_log_bytes: u64,
}

impl GuardianReplayPredecessorV1 {
    pub fn new(
        segment_id: Uuid,
        last_sequence: u64,
        terminal_record_digest: [u8; 32],
        cumulative_plaintext_bytes: u64,
        committed_log_bytes: u64,
    ) -> Result<Self, GuardianProtocolError> {
        let predecessor = Self {
            segment_id,
            last_sequence,
            terminal_record_digest,
            cumulative_plaintext_bytes,
            committed_log_bytes,
        };
        predecessor.validate()?;
        Ok(predecessor)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        if self.segment_id.is_nil()
            || self.last_sequence == 0
            || digest_is_zero(self.terminal_record_digest)
            || self.cumulative_plaintext_bytes == 0
            || self.committed_log_bytes == 0
        {
            Err(GuardianProtocolError::InvalidReplyPayload)
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub const fn segment_id(self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    #[must_use]
    pub const fn terminal_record_digest(self) -> [u8; 32] {
        self.terminal_record_digest
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianReplayRecordMetadataV1 {
    segment_id: Uuid,
    segment_first_sequence: u64,
    predecessor: Option<GuardianReplayPredecessorV1>,
    sequence: u64,
    payload_bytes: u32,
    cumulative_plaintext_bytes: u64,
    committed_log_bytes: u64,
    record_digest: [u8; 32],
}

impl GuardianReplayRecordMetadataV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        segment_id: Uuid,
        segment_first_sequence: u64,
        predecessor: Option<GuardianReplayPredecessorV1>,
        sequence: u64,
        payload_bytes: u32,
        cumulative_plaintext_bytes: u64,
        committed_log_bytes: u64,
        record_digest: [u8; 32],
    ) -> Result<Self, GuardianProtocolError> {
        let metadata = Self {
            segment_id,
            segment_first_sequence,
            predecessor,
            sequence,
            payload_bytes,
            cumulative_plaintext_bytes,
            committed_log_bytes,
            record_digest,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn validate(self) -> Result<(), GuardianProtocolError> {
        if self.segment_id.is_nil()
            || self.segment_first_sequence == 0
            || self.sequence < self.segment_first_sequence
            || self.payload_bytes == 0
            || self.payload_bytes > GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES
            || self.cumulative_plaintext_bytes < u64::from(self.payload_bytes)
            || self.committed_log_bytes == 0
            || digest_is_zero(self.record_digest)
            || self.predecessor.is_some_and(|predecessor| {
                predecessor.validate().is_err()
                    || predecessor
                        .last_sequence
                        .checked_add(1)
                        .is_none_or(|next| next != self.segment_first_sequence)
            })
            || (self.segment_first_sequence == 1) != self.predecessor.is_none()
        {
            Err(GuardianProtocolError::InvalidReplyPayload)
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub const fn segment_id(self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn segment_first_sequence(self) -> u64 {
        self.segment_first_sequence
    }

    #[must_use]
    pub const fn predecessor(self) -> Option<GuardianReplayPredecessorV1> {
        self.predecessor
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn payload_bytes(self) -> u32 {
        self.payload_bytes
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }

    #[must_use]
    pub const fn record_digest(self) -> [u8; 32] {
        self.record_digest
    }
}

#[derive(Debug, Error)]
pub enum GuardianReplayDeliveryError {
    #[error(transparent)]
    Protocol(#[from] GuardianProtocolError),
    #[error("guardian replay plaintext delivery failed")]
    Io(#[source] std::io::Error),
}

/// Single-use replay record. Plaintext stays in a zeroizing allocation and is
/// observable only by consuming the capability into a bounded writer.
pub struct GuardianReplayRecordDelivery {
    metadata: GuardianReplayRecordMetadataV1,
    plaintext_digest: [u8; 32],
    plaintext: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for GuardianReplayRecordDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianReplayRecordDelivery")
            .field("metadata", &self.metadata)
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

impl GuardianReplayRecordDelivery {
    pub fn new(
        metadata: GuardianReplayRecordMetadataV1,
        plaintext: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianProtocolError> {
        metadata.validate()?;
        if usize::try_from(metadata.payload_bytes).ok() != Some(plaintext.len()) {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let plaintext_digest = Sha256::digest(plaintext.as_slice()).into();
        Ok(Self {
            metadata,
            plaintext_digest,
            plaintext,
        })
    }

    #[must_use]
    pub const fn metadata(&self) -> GuardianReplayRecordMetadataV1 {
        self.metadata
    }

    pub fn write_all_bounded<W: std::io::Write>(
        self,
        writer: &mut W,
        max_payload_bytes: u32,
    ) -> Result<GuardianReplayRecordMetadataV1, GuardianReplayDeliveryError> {
        if max_payload_bytes == 0
            || self.metadata.payload_bytes > max_payload_bytes
            || usize::try_from(self.metadata.payload_bytes).ok() != Some(self.plaintext.len())
            || <[u8; 32]>::from(Sha256::digest(self.plaintext.as_slice()))
                != self.plaintext_digest
        {
            return Err(GuardianProtocolError::InvalidReplyPayload.into());
        }
        writer
            .write_all(self.plaintext.as_slice())
            .map_err(GuardianReplayDeliveryError::Io)?;
        Ok(self.metadata)
    }
}

pub struct GuardianReplayOutputRecordsDelivery {
    first_sequence: u64,
    records: Vec<GuardianReplayRecordDelivery>,
    plaintext_bytes: u32,
}

impl std::fmt::Debug for GuardianReplayOutputRecordsDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianReplayOutputRecordsDelivery")
            .field("first_sequence", &self.first_sequence)
            .field("record_count", &self.records.len())
            .field("plaintext_bytes", &self.plaintext_bytes)
            .finish()
    }
}

impl GuardianReplayOutputRecordsDelivery {
    pub fn new(
        first_sequence: u64,
        records: Vec<GuardianReplayRecordDelivery>,
    ) -> Result<Self, GuardianProtocolError> {
        let plaintext_bytes = validate_replay_records(first_sequence, &records)?;
        Ok(Self {
            first_sequence,
            records,
            plaintext_bytes,
        })
    }

    #[must_use]
    pub const fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub const fn plaintext_bytes(&self) -> u32 {
        self.plaintext_bytes
    }

    pub fn into_records(self) -> Vec<GuardianReplayRecordDelivery> {
        self.records
    }
}

fn validate_replay_records(
    first_sequence: u64,
    records: &[GuardianReplayRecordDelivery],
) -> Result<u32, GuardianProtocolError> {
    if first_sequence == 0
        || records.is_empty()
        || records.len() > usize::from(GUARDIAN_MAX_REPLAY_RECORDS)
        || records.first().map(|record| record.metadata.sequence) != Some(first_sequence)
    {
        return Err(GuardianProtocolError::InvalidReplyPayload);
    }
    let mut plaintext_bytes = 0_u32;
    let mut previous: Option<GuardianReplayRecordMetadataV1> = None;
    for record in records {
        let metadata = record.metadata;
        metadata.validate()?;
        if usize::try_from(metadata.payload_bytes).ok() != Some(record.plaintext.len())
            || <[u8; 32]>::from(Sha256::digest(record.plaintext.as_slice()))
                != record.plaintext_digest
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        plaintext_bytes = plaintext_bytes
            .checked_add(metadata.payload_bytes)
            .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
        if plaintext_bytes > GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        if let Some(prior) = previous {
            if prior.sequence.checked_add(1) != Some(metadata.sequence)
                || prior
                    .cumulative_plaintext_bytes
                    .checked_add(u64::from(metadata.payload_bytes))
                    != Some(metadata.cumulative_plaintext_bytes)
            {
                return Err(GuardianProtocolError::InvalidReplyPayload);
            }
            if prior.segment_id == metadata.segment_id {
                if prior.segment_first_sequence != metadata.segment_first_sequence
                    || prior.predecessor != metadata.predecessor
                    || metadata.committed_log_bytes <= prior.committed_log_bytes
                {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
            } else {
                let predecessor = metadata
                    .predecessor
                    .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
                if metadata.segment_first_sequence != metadata.sequence
                    || predecessor.segment_id != prior.segment_id
                    || predecessor.last_sequence != prior.sequence
                    || predecessor.terminal_record_digest != prior.record_digest
                    || predecessor.cumulative_plaintext_bytes
                        != prior.cumulative_plaintext_bytes
                    || predecessor.committed_log_bytes != prior.committed_log_bytes
                {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
            }
        } else if metadata.sequence == metadata.segment_first_sequence {
            let prior_cumulative = metadata
                .cumulative_plaintext_bytes
                .checked_sub(u64::from(metadata.payload_bytes))
                .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
            match metadata.predecessor {
                None if metadata.sequence == 1 && prior_cumulative == 0 => {}
                Some(predecessor)
                    if predecessor.last_sequence.checked_add(1) == Some(metadata.sequence)
                        && predecessor.cumulative_plaintext_bytes == prior_cumulative => {}
                _ => return Err(GuardianProtocolError::InvalidReplyPayload),
            }
        }
        previous = Some(metadata);
    }
    Ok(plaintext_bytes)
}

/// Single-use checkpoint chunk with the same zeroizing ownership contract as
/// output-record delivery.
pub struct GuardianCheckpointChunkDelivery {
    descriptor: GuardianCheckpointDescriptorV1,
    offset: u64,
    chunk_digest: [u8; 32],
    bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for GuardianCheckpointChunkDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointChunkDelivery")
            .field("descriptor", &self.descriptor)
            .field("offset", &self.offset)
            .field("chunk_bytes", &self.bytes.len())
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl GuardianCheckpointChunkDelivery {
    pub fn new(
        descriptor: GuardianCheckpointDescriptorV1,
        offset: u64,
        bytes: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianProtocolError> {
        descriptor.validate()?;
        let observed = u64::try_from(bytes.len())
            .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
        if bytes.is_empty()
            || observed > u64::from(GUARDIAN_MAX_RECOVERY_PLAINTEXT_BYTES)
            || offset
                .checked_add(observed)
                .is_none_or(|end| end > descriptor.total_bytes)
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let chunk_digest = Sha256::digest(bytes.as_slice()).into();
        Ok(Self {
            descriptor,
            offset,
            chunk_digest,
            bytes,
        })
    }

    #[must_use]
    pub const fn descriptor(&self) -> GuardianCheckpointDescriptorV1 {
        self.descriptor
    }

    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn chunk_digest(&self) -> [u8; 32] {
        self.chunk_digest
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn write_all_bounded<W: std::io::Write>(
        self,
        writer: &mut W,
        max_payload_bytes: u32,
    ) -> Result<(GuardianCheckpointDescriptorV1, u64, u32), GuardianReplayDeliveryError> {
        let observed = u32::try_from(self.bytes.len())
            .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
        if max_payload_bytes == 0
            || observed > max_payload_bytes
            || <[u8; 32]>::from(Sha256::digest(self.bytes.as_slice())) != self.chunk_digest
        {
            return Err(GuardianProtocolError::InvalidReplyPayload.into());
        }
        writer
            .write_all(self.bytes.as_slice())
            .map_err(GuardianReplayDeliveryError::Io)?;
        Ok((self.descriptor, self.offset, observed))
    }
}

struct GuardianBoundedPayloadBuffer {
    bytes: Zeroizing<Vec<u8>>,
    max_bytes: usize,
    exceeded: bool,
}

impl GuardianBoundedPayloadBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::new()),
            max_bytes,
            exceeded: false,
        }
    }

    fn into_inner(mut self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::mem::take(&mut *self.bytes))
    }
}

impl std::io::Write for GuardianBoundedPayloadBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("guardian payload length overflow"))?;
        if next > self.max_bytes {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "guardian payload serialization exceeded its byte ceiling",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| std::io::Error::other(format!("guardian payload reserve: {error}")))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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

    pub fn success_reply(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if self.0.header.status != GuardianResponseStatus::Success {
            return Err(GuardianProtocolError::NonSuccessResponse);
        }
        self.typed_reply(request)
    }

    /// Decode an authenticated typed reply, including a checkpoint whose
    /// publication outcome is explicitly indeterminate. Rejections remain on
    /// the separate rejection-code path.
    pub fn typed_reply(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if !matches!(
            self.0.header.status,
            GuardianResponseStatus::Success | GuardianResponseStatus::Indeterminate
        ) {
            return Err(GuardianProtocolError::NonSuccessResponse);
        }
        let header = &self.0.header;
        let request_header = &request.header;
        if header.protocol_version != request_header.protocol_version
            || header.operation != request_header.operation
            || header.guardian_incarnation != request_header.guardian_incarnation
            || header.mux_incarnation != request_header.mux_incarnation
            || header.request_id != request_header.request_id
            || header.request_payload_sha256 != request_header.payload_sha256
            || header.pane_id != request_header.pane_id
            || header.lease_generation != request_header.lease_generation
            || header.lease_sequence != request_header.lease_sequence
            || header.effect_id != request_header.effect_id
        {
            return Err(GuardianProtocolError::ResponseRequestMismatch);
        }
        let reply = GuardianReply::decode_for_operation(self.0.header.operation, &self.0.payload)?;
        if reply.response_status() != self.0.header.status {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        reply.require_response_identity(&self.0.header)?;
        reply.require_request_payload(request)?;
        Ok(reply)
    }

    pub fn rejection_code(&self) -> Result<GuardianRejectionCode, GuardianProtocolError> {
        GuardianRejectionCode::decode(self.0.header.status, &self.0.payload)
    }
}

impl AuthenticatedGuardianRequest {
    #[must_use]
    pub const fn header(&self) -> &GuardianRequestHeader {
        &self.envelope.header
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.envelope.payload
    }

    #[must_use]
    pub const fn envelope(&self) -> &GuardianRequestEnvelope {
        &self.envelope
    }

    /// Original authenticated payload length, retained after plaintext wipe.
    #[must_use]
    pub const fn authenticated_payload_bytes(&self) -> u32 {
        self.authenticated_payload_bytes
    }

    /// Wipe the authenticated request payload as soon as its operation has
    /// consumed it. The authenticated header and payload commitment remain
    /// available for response correlation.
    pub fn zeroize_payload(&mut self) {
        self.envelope.payload.zeroize();
    }

    /// Consume the authenticated envelope and transfer its sensitive payload
    /// into an allocation that remains zeroizing at the next ownership layer.
    pub fn into_zeroizing_payload(mut self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::mem::take(&mut self.envelope.payload))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianCensusPageRequest {
    /// Nil only for cursor zero to request a new guardian-allocated snapshot;
    /// continuation pages echo the nonzero UUID returned by the first reply.
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
        let snapshot_identity_is_valid = if self.cursor == 0 {
            self.snapshot_id.is_nil()
        } else {
            !self.snapshot_id.is_nil()
        };
        if !snapshot_identity_is_valid
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
    DurableFull,
    DurablePrefix { applied_bytes: u32 },
    KnownNotApplied,
    DispositionUnavailable,
}

/// Exact authority for completing one authenticated input effect.
///
/// Effect UUIDs may be reused only after their bounded receipt rotates and a
/// later generation/sequence fence makes the old mutation impossible. Binding
/// runtime durability completion to the full authenticated fingerprint keeps a
/// delayed journal acknowledgement for that old UUID from completing a newer
/// input that happens to reuse it.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianInputEffectIdentity {
    pane_id: Uuid,
    mux_incarnation: Uuid,
    generation: u64,
    sequence: u64,
    effect_id: Uuid,
    input_bytes: u32,
    payload_sha256: [u8; 32],
}

impl std::fmt::Debug for GuardianInputEffectIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianInputEffectIdentity")
            .field("pane_id", &self.pane_id)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("effect_id", &self.effect_id)
            .field("input_bytes", &self.input_bytes)
            .finish_non_exhaustive()
    }
}

impl GuardianInputEffectIdentity {
    pub fn new(
        pane_id: Uuid,
        mux_incarnation: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
        input_bytes: u32,
        payload_sha256: [u8; 32],
    ) -> Result<Self, GuardianProtocolError> {
        require_nonzero(pane_id, "pane id")?;
        require_nonzero(mux_incarnation, "mux incarnation")?;
        require_nonzero(effect_id, "effect id")?;
        if generation == 0
            || sequence == 0
            || input_bytes == 0
            || usize::try_from(input_bytes).map_or(true, |bytes| bytes > GUARDIAN_MAX_INPUT_BYTES)
        {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        Ok(Self {
            pane_id,
            mux_incarnation,
            generation,
            sequence,
            effect_id,
            input_bytes,
            payload_sha256,
        })
    }

    pub fn from_authenticated_request(
        request: &AuthenticatedGuardianRequest,
    ) -> Result<Self, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.operation != GuardianOperation::Input {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        Self::new(
            request
                .header
                .pane_id
                .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?,
            request.header.mux_incarnation,
            request.header.lease_generation,
            request.header.lease_sequence,
            request
                .header
                .effect_id
                .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?,
            u32::try_from(request.payload.len())
                .map_err(|_| GuardianProtocolError::InputDurabilityIdentityMismatch)?,
            request.header.payload_sha256,
        )
    }

    #[must_use]
    pub const fn pane_id(self) -> Uuid {
        self.pane_id
    }

    #[must_use]
    pub const fn mux_incarnation(self) -> Uuid {
        self.mux_incarnation
    }

    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn effect_id(self) -> Uuid {
        self.effect_id
    }

    #[must_use]
    pub const fn input_bytes(self) -> u32 {
        self.input_bytes
    }

    #[must_use]
    pub const fn payload_sha256(self) -> [u8; 32] {
        self.payload_sha256
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianInputEffectQuery {
    sequence: u64,
    input_bytes: u32,
    payload_sha256: [u8; 32],
}

impl std::fmt::Debug for GuardianInputEffectQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianInputEffectQuery")
            .field("sequence", &self.sequence)
            .field("input_bytes", &self.input_bytes)
            .finish_non_exhaustive()
    }
}

impl GuardianInputEffectQuery {
    pub fn new(
        sequence: u64,
        input_bytes: u32,
        payload_sha256: [u8; 32],
    ) -> Result<Self, GuardianProtocolError> {
        if sequence == 0
            || input_bytes == 0
            || usize::try_from(input_bytes).map_or(true, |bytes| bytes > GUARDIAN_MAX_INPUT_BYTES)
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(Self {
            sequence,
            input_bytes,
            payload_sha256,
        })
    }

    #[must_use]
    pub fn encode(self) -> [u8; INPUT_EFFECT_QUERY_PAYLOAD_BYTES] {
        let mut payload = [0_u8; INPUT_EFFECT_QUERY_PAYLOAD_BYTES];
        payload[..4].copy_from_slice(&INPUT_EFFECT_QUERY_PAYLOAD_MAGIC);
        payload[4..12].copy_from_slice(&self.sequence.to_be_bytes());
        payload[12..16].copy_from_slice(&self.input_bytes.to_be_bytes());
        payload[16..].copy_from_slice(&self.payload_sha256);
        payload
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != INPUT_EFFECT_QUERY_PAYLOAD_BYTES
            || payload[..4] != INPUT_EFFECT_QUERY_PAYLOAD_MAGIC
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let mut payload_sha256 = [0_u8; 32];
        payload_sha256.copy_from_slice(&payload[16..]);
        Self::new(
            read_u64(payload, 4)?,
            read_u32(payload, 12)?,
            payload_sha256,
        )
    }
}

impl InputEffectState {
    const fn to_wire(self) -> (u8, u32) {
        match self {
            Self::NotSeen => (0, 0),
            Self::AcceptedNotDurable => (1, 0),
            Self::DurableFull => (2, 0),
            Self::KnownNotApplied => (3, 0),
            Self::DispositionUnavailable => (4, 0),
            Self::DurablePrefix { applied_bytes } => (5, applied_bytes),
        }
    }

    fn from_wire(value: u8, applied_bytes: u32) -> Result<Self, GuardianProtocolError> {
        match (value, applied_bytes) {
            (0, 0) => Ok(Self::NotSeen),
            (1, 0) => Ok(Self::AcceptedNotDurable),
            (2, 0) => Ok(Self::DurableFull),
            (3, 0) => Ok(Self::KnownNotApplied),
            (4, 0) => Ok(Self::DispositionUnavailable),
            (5, applied_bytes)
                if applied_bytes > 0
                    && usize::try_from(applied_bytes)
                        .is_ok_and(|bytes| bytes <= GUARDIAN_MAX_INPUT_BYTES) =>
            {
                Ok(Self::DurablePrefix { applied_bytes })
            }
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
    }

    fn is_input_receipt(self) -> bool {
        matches!(
            self,
            Self::AcceptedNotDurable
                | Self::DurableFull
                | Self::DurablePrefix { .. }
                | Self::KnownNotApplied
        ) && self.has_canonical_wire_count()
    }

    fn has_canonical_wire_count(self) -> bool {
        !matches!(
            self,
            Self::DurablePrefix { applied_bytes }
                if applied_bytes == 0
                    || usize::try_from(applied_bytes)
                        .map_or(true, |bytes| bytes > GUARDIAN_MAX_INPUT_BYTES)
        )
    }

    fn validate_for_input_bytes(self, input_bytes: u32) -> Result<(), GuardianProtocolError> {
        if input_bytes == 0
            || usize::try_from(input_bytes)
                .map_or(true, |bytes| bytes > GUARDIAN_MAX_INPUT_BYTES)
        {
            return Err(GuardianProtocolError::InvalidInputDisposition);
        }
        if matches!(
            self,
            Self::DurablePrefix { applied_bytes }
                if applied_bytes == 0 || applied_bytes >= input_bytes
        ) {
            return Err(GuardianProtocolError::InvalidInputDisposition);
        }
        Ok(())
    }
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
    pub indeterminate_checkpoint_effect: Option<Uuid>,
    pub exit_status: Option<i32>,
    pub quarantine_reason: Option<GuardianQuarantineReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuardianReply {
    Hello {
        guardian_incarnation: Uuid,
    },
    GuardedStopAccepted,
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
    CheckpointReceipt(GuardianCheckpointReceipt),
    MutationApplied { pane_id: Uuid, generation: u64, sequence: u64 },
    LeaseRetired { pane_id: Uuid, generation: u64 },
    InputEffect { effect_id: Uuid, state: InputEffectState },
    ReplayReady { pane_id: Uuid, generation: u64 },
    /// The exact authenticated effect identity was committed, but its external
    /// callback may or may not have applied. The pane is permanently
    /// quarantined; this receipt is diagnostic/reconciliation authority, never
    /// permission to retry the external effect.
    EffectOutcomeIndeterminate {
        pane_id: Uuid,
        generation: u64,
        sequence: u64,
        effect_id: Uuid,
    },
}

impl GuardianReply {
    const fn response_status(&self) -> GuardianResponseStatus {
        match self {
            Self::CheckpointReceipt(receipt)
                if matches!(
                    receipt.disposition,
                    GuardianCheckpointDisposition::OutcomeIndeterminate
                ) => GuardianResponseStatus::Indeterminate,
            Self::EffectOutcomeIndeterminate { .. } => GuardianResponseStatus::Indeterminate,
            _ => GuardianResponseStatus::Success,
        }
    }

    pub fn effect_outcome_indeterminate(
        request: &AuthenticatedGuardianRequest,
        intended_reply: &Self,
    ) -> Result<Self, GuardianProtocolError> {
        if !request.header.operation.supports_generic_effect_indeterminate() {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        intended_reply.require_response_identity(&GuardianResponseHeader::new(
            &request.header,
            GuardianResponseStatus::Success,
            &intended_reply.encode_for_operation(request.header.operation)?,
        ))?;
        let effect_id = request
            .header
            .effect_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        let (pane_id, generation, sequence) = match intended_reply {
            Self::Spawned {
                pane_id,
                generation,
            } => (*pane_id, *generation, 0),
            Self::Claimed {
                pane_id,
                generation,
                next_sequence,
            } => (*pane_id, *generation, *next_sequence),
            Self::MutationApplied {
                pane_id,
                generation,
                sequence,
            } => (*pane_id, *generation, *sequence),
            Self::LeaseRetired {
                pane_id,
                generation,
            } => (*pane_id, *generation, request.header.lease_sequence),
            _ => return Err(GuardianProtocolError::InvalidReplyPayload),
        };
        let receipt = Self::EffectOutcomeIndeterminate {
            pane_id,
            generation,
            sequence,
            effect_id,
        };
        receipt.require_operation(request.header.operation)?;
        Ok(receipt)
    }

    pub fn encode_for_operation(
        &self,
        operation: GuardianOperation,
    ) -> Result<Vec<u8>, GuardianProtocolError> {
        self.require_operation(operation)?;
        let capacity = match self {
            Self::Hello { .. } => 16,
            Self::GuardedStopAccepted => 0,
            Self::CensusPage { entries, .. } => usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES)
                .ok()
                .and_then(|header| {
                    usize::try_from(GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES)
                        .ok()
                        .and_then(|entry| entry.checked_mul(entries.len()))
                        .and_then(|entries| header.checked_add(entries))
                })
                .ok_or(GuardianProtocolError::PayloadTooLarge)?,
            Self::Spawned { .. } | Self::LeaseRetired { .. } | Self::ReplayReady { .. } => 24,
            Self::Claimed { .. }
            | Self::Attached { .. }
            | Self::MutationApplied { .. } => 32,
            Self::InputReceipt { .. } => INPUT_RECEIPT_PAYLOAD_BYTES,
            Self::CheckpointReceipt(..) => GUARDIAN_CHECKPOINT_RECEIPT_BYTES,
            Self::InputEffect { .. } => INPUT_EFFECT_REPLY_PAYLOAD_BYTES,
            Self::EffectOutcomeIndeterminate { .. } => 48,
        };
        if capacity > GUARDIAN_MAX_PAYLOAD_BYTES {
            return Err(GuardianProtocolError::PayloadTooLarge);
        }
        let mut payload = Vec::with_capacity(capacity);
        match self {
            Self::Hello {
                guardian_incarnation,
            } => push_uuid(&mut payload, *guardian_incarnation),
            Self::GuardedStopAccepted => {}
            Self::Spawned {
                pane_id,
                generation,
            }
            | Self::LeaseRetired {
                pane_id,
                generation,
            }
            | Self::ReplayReady {
                pane_id,
                generation,
            } => {
                push_uuid(&mut payload, *pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
            }
            Self::CensusPage {
                snapshot_id,
                entries,
                next_cursor,
                total_panes,
            } => {
                if snapshot_id.is_nil()
                    || entries.len() > usize::from(GUARDIAN_MAX_CENSUS_ENTRIES)
                    || u64::try_from(entries.len()).unwrap_or(u64::MAX) > *total_panes
                {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                push_uuid(&mut payload, *snapshot_id);
                payload.extend_from_slice(&next_cursor.unwrap_or(u64::MAX).to_be_bytes());
                payload.extend_from_slice(&total_panes.to_be_bytes());
                payload.extend_from_slice(
                    &u16::try_from(entries.len())
                        .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?
                        .to_be_bytes(),
                );
                for entry in entries {
                    entry.encode_into(&mut payload)?;
                }
            }
            Self::Claimed {
                pane_id,
                generation,
                next_sequence,
            }
            | Self::Attached {
                pane_id,
                generation,
                next_sequence,
            } => {
                push_uuid(&mut payload, *pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
                payload.extend_from_slice(&next_sequence.to_be_bytes());
            }
            Self::InputReceipt {
                pane_id,
                generation,
                sequence,
                effect_id,
                state,
            } => {
                push_uuid(&mut payload, *pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
                payload.extend_from_slice(&sequence.to_be_bytes());
                push_uuid(&mut payload, *effect_id);
                let (disposition, applied_bytes) = state.to_wire();
                payload.push(disposition);
                payload.extend_from_slice(&applied_bytes.to_be_bytes());
            }
            Self::CheckpointReceipt(receipt) => payload.extend_from_slice(&receipt.encode()),
            Self::MutationApplied {
                pane_id,
                generation,
                sequence,
            } => {
                push_uuid(&mut payload, *pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
                payload.extend_from_slice(&sequence.to_be_bytes());
            }
            Self::InputEffect { effect_id, state } => {
                push_uuid(&mut payload, *effect_id);
                let (disposition, applied_bytes) = state.to_wire();
                payload.push(disposition);
                payload.extend_from_slice(&applied_bytes.to_be_bytes());
            }
            Self::EffectOutcomeIndeterminate {
                pane_id,
                generation,
                sequence,
                effect_id,
            } => {
                push_uuid(&mut payload, *pane_id);
                payload.extend_from_slice(&generation.to_be_bytes());
                payload.extend_from_slice(&sequence.to_be_bytes());
                push_uuid(&mut payload, *effect_id);
            }
        }
        if payload.len() != capacity {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian-reply-encoded-size",
            ));
        }
        Ok(payload)
    }

    pub fn decode_for_operation(
        operation: GuardianOperation,
        payload: &[u8],
    ) -> Result<Self, GuardianProtocolError> {
        if payload.len() > GUARDIAN_MAX_PAYLOAD_BYTES {
            return Err(GuardianProtocolError::PayloadTooLarge);
        }
        if operation.supports_generic_effect_indeterminate() && payload.len() == 48 {
            let reply = Self::EffectOutcomeIndeterminate {
                pane_id: read_required_uuid(payload, 0)?,
                generation: read_u64(payload, 16)?,
                sequence: read_u64(payload, 24)?,
                effect_id: read_required_uuid(payload, 32)?,
            };
            reply.require_operation(operation)?;
            return Ok(reply);
        }
        let reply = match operation {
            GuardianOperation::Hello => {
                require_reply_len(payload, 16)?;
                Self::Hello {
                    guardian_incarnation: read_required_uuid(payload, 0)?,
                }
            }
            GuardianOperation::GuardedStop => {
                require_reply_len(payload, 0)?;
                Self::GuardedStopAccepted
            }
            GuardianOperation::Spawn => {
                require_reply_len(payload, 24)?;
                Self::Spawned {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                }
            }
            GuardianOperation::Census => {
                let header_bytes = usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES)
                    .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
                if payload.len() < header_bytes {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                let snapshot_id = read_required_uuid(payload, 0)?;
                let cursor = read_u64(payload, 16)?;
                let next_cursor = (cursor != u64::MAX).then_some(cursor);
                let total_panes = read_u64(payload, 24)?;
                let count = usize::from(read_u16(payload, 32)?);
                if count > usize::from(GUARDIAN_MAX_CENSUS_ENTRIES) {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                let entry_bytes = usize::try_from(GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES)
                    .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
                let expected = entry_bytes
                    .checked_mul(count)
                    .and_then(|entries| header_bytes.checked_add(entries))
                    .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
                require_reply_len(payload, expected)?;
                if u64::try_from(count).unwrap_or(u64::MAX) > total_panes {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                let mut entries = Vec::with_capacity(count);
                for index in 0..count {
                    let offset = header_bytes
                        .checked_add(
                            entry_bytes
                                .checked_mul(index)
                                .ok_or(GuardianProtocolError::InvalidReplyPayload)?,
                        )
                        .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
                    entries.push(GuardianCensusEntry::decode_from(
                        payload
                            .get(offset..offset + entry_bytes)
                            .ok_or(GuardianProtocolError::InvalidReplyPayload)?,
                    )?);
                }
                Self::CensusPage {
                    snapshot_id,
                    entries,
                    next_cursor,
                    total_panes,
                }
            }
            GuardianOperation::Claim | GuardianOperation::Attach => {
                require_reply_len(payload, 32)?;
                let pane_id = read_required_uuid(payload, 0)?;
                let generation = read_u64(payload, 16)?;
                let next_sequence = read_u64(payload, 24)?;
                if generation == 0 || next_sequence == 0 {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                if operation == GuardianOperation::Claim {
                    Self::Claimed {
                        pane_id,
                        generation,
                        next_sequence,
                    }
                } else {
                    Self::Attached {
                        pane_id,
                        generation,
                        next_sequence,
                    }
                }
            }
            GuardianOperation::Input => {
                require_reply_len(payload, INPUT_RECEIPT_PAYLOAD_BYTES)?;
                Self::InputReceipt {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                    sequence: read_u64(payload, 24)?,
                    effect_id: read_required_uuid(payload, 32)?,
                    state: InputEffectState::from_wire(payload[48], read_u32(payload, 49)?)?,
                }
            }
            GuardianOperation::Resize
            | GuardianOperation::Signal
            | GuardianOperation::Close => {
                require_reply_len(payload, 32)?;
                Self::MutationApplied {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                    sequence: read_u64(payload, 24)?,
                }
            }
            GuardianOperation::Checkpoint => {
                Self::CheckpointReceipt(GuardianCheckpointReceipt::decode(payload)?)
            }
            GuardianOperation::Replay => {
                require_reply_len(payload, 24)?;
                Self::ReplayReady {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                }
            }
            GuardianOperation::QueryInputEffect => {
                require_reply_len(payload, INPUT_EFFECT_REPLY_PAYLOAD_BYTES)?;
                Self::InputEffect {
                    effect_id: read_required_uuid(payload, 0)?,
                    state: InputEffectState::from_wire(payload[16], read_u32(payload, 17)?)?,
                }
            }
            GuardianOperation::RetireLease => {
                require_reply_len(payload, 24)?;
                Self::LeaseRetired {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                }
            }
        };
        reply.require_operation(operation)?;
        Ok(reply)
    }

    fn require_operation(
        &self,
        operation: GuardianOperation,
    ) -> Result<(), GuardianProtocolError> {
        let matches = matches!(
            (operation, self),
            (GuardianOperation::Hello, Self::Hello { .. })
                | (
                    GuardianOperation::GuardedStop,
                    Self::GuardedStopAccepted
                )
                | (GuardianOperation::Spawn, Self::Spawned { .. })
                | (GuardianOperation::Census, Self::CensusPage { .. })
                | (GuardianOperation::Claim, Self::Claimed { .. })
                | (GuardianOperation::Attach, Self::Attached { .. })
                | (GuardianOperation::Input, Self::InputReceipt { .. })
                | (
                    GuardianOperation::Checkpoint,
                    Self::CheckpointReceipt(..)
                )
                | (
                    GuardianOperation::Resize
                        | GuardianOperation::Signal
                        | GuardianOperation::Close,
                    Self::MutationApplied { .. }
                )
                | (GuardianOperation::Replay, Self::ReplayReady { .. })
                | (
                    GuardianOperation::QueryInputEffect,
                    Self::InputEffect { .. }
                )
                | (GuardianOperation::RetireLease, Self::LeaseRetired { .. })
                | (
                    GuardianOperation::Spawn
                        | GuardianOperation::Claim
                        | GuardianOperation::Resize
                        | GuardianOperation::Signal
                        | GuardianOperation::Close
                        | GuardianOperation::RetireLease,
                    Self::EffectOutcomeIndeterminate { .. }
                )
        );
        if matches {
            let valid = match self {
                Self::Hello {
                    guardian_incarnation,
                } => !guardian_incarnation.is_nil(),
                Self::GuardedStopAccepted => true,
                Self::Spawned {
                    pane_id,
                    generation,
                } => !pane_id.is_nil() && *generation == 0,
                Self::CensusPage {
                    snapshot_id,
                    entries,
                    next_cursor,
                    total_panes,
                } => {
                    !snapshot_id.is_nil()
                        && *total_panes <= u64::try_from(GUARDIAN_MAX_PANES).unwrap_or(u64::MAX)
                        && u64::try_from(entries.len()).unwrap_or(u64::MAX) <= *total_panes
                        && next_cursor.is_none_or(|cursor| {
                            cursor > 0 && cursor < *total_panes && !entries.is_empty()
                        })
                        && entries
                            .windows(2)
                            .all(|pair| pair[0].pane_id < pair[1].pane_id)
                        && entries
                            .iter()
                            .all(|entry| entry.validate_wire_shape().is_ok())
                }
                Self::Claimed {
                    pane_id,
                    generation,
                    next_sequence,
                }
                | Self::Attached {
                    pane_id,
                    generation,
                    next_sequence,
                } => !pane_id.is_nil() && *generation > 0 && *next_sequence > 0,
                Self::InputReceipt {
                    pane_id,
                    generation,
                    sequence,
                    effect_id,
                    state,
                } => {
                    !pane_id.is_nil()
                        && *generation > 0
                        && *sequence > 0
                        && !effect_id.is_nil()
                        && state.is_input_receipt()
                }
                Self::CheckpointReceipt(receipt) => {
                    !receipt.pane_id.is_nil()
                        && receipt.generation > 0
                        && receipt.sequence > 0
                        && !receipt.effect_id.is_nil()
                }
                Self::MutationApplied {
                    pane_id,
                    generation,
                    sequence,
                } => {
                    !pane_id.is_nil()
                        && if operation == GuardianOperation::Close && *sequence == 0 {
                            true
                        } else {
                            *generation > 0 && *sequence > 0
                        }
                }
                Self::LeaseRetired {
                    pane_id,
                    generation,
                } => !pane_id.is_nil() && *generation > 0,
                Self::InputEffect { effect_id, state } => {
                    !effect_id.is_nil() && state.has_canonical_wire_count()
                }
                Self::ReplayReady { pane_id, .. } => !pane_id.is_nil(),
                Self::EffectOutcomeIndeterminate {
                    pane_id,
                    generation,
                    sequence,
                    effect_id,
                } => {
                    !pane_id.is_nil()
                        && !effect_id.is_nil()
                        && match operation {
                            GuardianOperation::Spawn => *generation == 0 && *sequence == 0,
                            GuardianOperation::Claim => *generation > 0 && *sequence > 0,
                            GuardianOperation::Resize
                            | GuardianOperation::Signal
                            | GuardianOperation::RetireLease => {
                                *generation > 0 && *sequence > 0
                            }
                            GuardianOperation::Close => {
                                (*generation > 0 && *sequence > 0) || *sequence == 0
                            }
                            _ => false,
                        }
                }
            };
            if valid {
                Ok(())
            } else {
                Err(GuardianProtocolError::InvalidReplyPayload)
            }
        } else {
            Err(GuardianProtocolError::ReplyOperationMismatch { operation })
        }
    }

    fn require_response_identity(
        &self,
        header: &GuardianResponseHeader,
    ) -> Result<(), GuardianProtocolError> {
        self.require_operation(header.operation)?;
        let matches = match self {
            Self::Hello { .. } => {
                header.guardian_incarnation.is_nil()
                    && header.pane_id.is_none()
                    && header.effect_id.is_none()
            }
            Self::GuardedStopAccepted => {
                header.pane_id.is_none()
                    && header.effect_id.is_some()
                    && header.lease_generation == 0
                    && header.lease_sequence == 0
            }
            Self::Spawned {
                pane_id,
                generation,
            } => header.pane_id == Some(*pane_id) && *generation == 0,
            Self::CensusPage { .. } => header.pane_id.is_none() && header.effect_id.is_none(),
            Self::Claimed {
                pane_id,
                generation,
                next_sequence,
            } => {
                header.pane_id == Some(*pane_id)
                    && header
                        .lease_generation
                        .checked_add(1)
                        .is_some_and(|expected| expected == *generation)
                    && *next_sequence == 1
            }
            Self::Attached {
                pane_id,
                generation,
                ..
            } => header.pane_id == Some(*pane_id) && header.lease_generation == *generation,
            Self::InputReceipt {
                pane_id,
                generation,
                sequence,
                effect_id,
                ..
            } => {
                header.pane_id == Some(*pane_id)
                    && header.lease_generation == *generation
                    && header.lease_sequence == *sequence
                    && header.effect_id == Some(*effect_id)
            }
            Self::CheckpointReceipt(receipt) => {
                header.pane_id == Some(receipt.pane_id)
                    && header.lease_generation == receipt.generation
                    && header.lease_sequence == receipt.sequence
                    && header.effect_id == Some(receipt.effect_id)
            }
            Self::MutationApplied {
                pane_id,
                generation,
                sequence,
            } => {
                header.pane_id == Some(*pane_id)
                    && header.lease_generation == *generation
                    && header.lease_sequence == *sequence
            }
            Self::LeaseRetired {
                pane_id,
                generation,
            }
            | Self::ReplayReady {
                pane_id,
                generation,
            } => header.pane_id == Some(*pane_id) && header.lease_generation == *generation,
            Self::InputEffect { effect_id, .. } => header.effect_id == Some(*effect_id),
            Self::EffectOutcomeIndeterminate {
                pane_id,
                generation,
                sequence,
                effect_id,
            } => {
                header.pane_id == Some(*pane_id)
                    && header.effect_id == Some(*effect_id)
                    && match header.operation {
                        GuardianOperation::Spawn => {
                            header.lease_generation == 0
                                && header.lease_sequence == 0
                                && *generation == 0
                                && *sequence == 0
                        }
                        GuardianOperation::Claim => {
                            header
                                .lease_generation
                                .checked_add(1)
                                .is_some_and(|expected| expected == *generation)
                                && header.lease_sequence == 0
                                && *sequence == 1
                        }
                        GuardianOperation::Resize
                        | GuardianOperation::Signal
                        | GuardianOperation::Close
                        | GuardianOperation::RetireLease => {
                            header.lease_generation == *generation
                                && header.lease_sequence == *sequence
                        }
                        _ => false,
                    }
            }
        };
        if matches {
            Ok(())
        } else {
            Err(GuardianProtocolError::ResponseRequestMismatch)
        }
    }

    fn require_request_payload(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<(), GuardianProtocolError> {
        match self {
            Self::InputReceipt { state, .. } => {
                state.validate_for_input_bytes(request.authenticated_payload_bytes())
            }
            Self::InputEffect { state, .. } => {
                let query = GuardianInputEffectQuery::decode(&request.payload)?;
                state.validate_for_input_bytes(query.input_bytes)
            }
            Self::CensusPage {
                snapshot_id,
                entries,
                next_cursor,
                total_panes,
            } => {
                let page = GuardianCensusPageRequest::decode(&request.payload)?;
                let entry_count = u64::try_from(entries.len())
                    .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
                let encoded_bytes = GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES
                    .checked_mul(
                        u32::try_from(entries.len())
                            .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
                    )
                    .and_then(|entries_bytes| {
                        GUARDIAN_CENSUS_PAGE_HEADER_BYTES.checked_add(entries_bytes)
                    })
                    .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
                let end = page
                    .cursor
                    .checked_add(entry_count)
                    .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
                let expected_next = (end < *total_panes).then_some(end);
                if entries.len() > usize::from(page.max_entries)
                    || encoded_bytes > page.max_bytes
                    || (page.cursor == 0 && snapshot_id.is_nil())
                    || (page.cursor > 0 && *snapshot_id != page.snapshot_id)
                    || page.cursor > *total_panes
                    || end > *total_panes
                    || (end < *total_panes && entries.is_empty())
                    || *next_cursor != expected_next
                {
                    return Err(GuardianProtocolError::InvalidReplyPayload);
                }
                Ok(())
            }
            Self::CheckpointReceipt(receipt) => {
                let identity = GuardianCheckpointEffectIdentity::from_authenticated_request(request)?;
                if receipt.matches_identity(identity) {
                    Ok(())
                } else {
                    Err(GuardianProtocolError::ResponseRequestMismatch)
                }
            }
            _ => Ok(()),
        }
    }
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

/// Result of retiring leases after the transport layer proves that one mux
/// incarnation has no remaining live authenticated connections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardianMuxLeaseRetirement {
    pub retired_panes: usize,
    pub pending_input_panes: usize,
    pub indeterminate_checkpoint_panes: usize,
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
    fn from_state(
        pane_id: Uuid,
        state: &GuardianPaneState,
        indeterminate_checkpoint_effect: Option<Uuid>,
    ) -> Self {
        match state {
            GuardianPaneState::LiveUnclaimed { generation } => Self {
                pane_id,
                status: GuardianCensusPaneStatus::LiveUnclaimed,
                generation: *generation,
                mux_incarnation: None,
                next_sequence: None,
                pending_input_effect: None,
                indeterminate_checkpoint_effect,
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
                indeterminate_checkpoint_effect,
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
                indeterminate_checkpoint_effect,
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
                indeterminate_checkpoint_effect,
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
                indeterminate_checkpoint_effect,
                exit_status: *exit_status,
                quarantine_reason: Some(*reason),
            },
        }
    }

    fn encode_into(&self, payload: &mut Vec<u8>) -> Result<(), GuardianProtocolError> {
        self.validate_wire_shape()?;
        let start = payload.len();
        push_uuid(payload, self.pane_id);
        payload.push(self.status.to_wire());
        payload.extend_from_slice(&self.generation.to_be_bytes());
        push_optional_uuid(payload, self.mux_incarnation);
        payload.extend_from_slice(&self.next_sequence.unwrap_or(0).to_be_bytes());
        push_optional_uuid(payload, self.pending_input_effect);
        push_optional_uuid(payload, self.indeterminate_checkpoint_effect);
        payload.extend_from_slice(&self.exit_status.unwrap_or(0).to_be_bytes());
        payload.push(
            self.quarantine_reason
                .map_or(0, GuardianQuarantineReason::to_wire),
        );
        payload.push(u8::from(self.exit_status.is_some()));
        if payload.len().saturating_sub(start)
            != usize::try_from(GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES).unwrap_or(usize::MAX)
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian-census-entry-encoded-size",
            ));
        }
        Ok(())
    }

    fn decode_from(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        require_reply_len(
            payload,
            usize::try_from(GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES)
                .map_err(|_| GuardianProtocolError::InvalidReplyPayload)?,
        )?;
        let status = GuardianCensusPaneStatus::from_wire(payload[16])?;
        let next_sequence = match read_u64(payload, 41)? {
            0 => None,
            value => Some(value),
        };
        let flags = payload[86];
        if flags & !1 != 0 {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let encoded_exit_status = read_i32(payload, 81)?;
        let exit_status = if flags & 1 == 0 {
            if encoded_exit_status != 0 {
                return Err(GuardianProtocolError::InvalidReplyPayload);
            }
            None
        } else {
            Some(encoded_exit_status)
        };
        let entry = Self {
            pane_id: read_required_uuid(payload, 0)?,
            status,
            generation: read_u64(payload, 17)?,
            mux_incarnation: read_optional_uuid(payload, 25)?,
            next_sequence,
            pending_input_effect: read_optional_uuid(payload, 49)?,
            indeterminate_checkpoint_effect: read_optional_uuid(payload, 65)?,
            exit_status,
            quarantine_reason: GuardianQuarantineReason::from_wire(payload[85])?,
        };
        entry.validate_wire_shape()?;
        Ok(entry)
    }

    fn validate_wire_shape(&self) -> Result<(), GuardianProtocolError> {
        if self.pane_id.is_nil()
            || self.next_sequence == Some(0)
            || self.mux_incarnation.is_some_and(|value| value.is_nil())
            || self.pending_input_effect.is_some_and(|value| value.is_nil())
            || self
                .indeterminate_checkpoint_effect
                .is_some_and(|value| value.is_nil())
            || (self.pending_input_effect.is_some()
                && self.indeterminate_checkpoint_effect.is_some())
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let valid = match self.status {
            GuardianCensusPaneStatus::LiveUnclaimed => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.pending_input_effect.is_none()
                    && self.indeterminate_checkpoint_effect.is_none()
                    && self.exit_status.is_none()
                    && self.quarantine_reason.is_none()
            }
            GuardianCensusPaneStatus::LiveClaimed => {
                self.generation > 0
                    && self.mux_incarnation.is_some()
                    && self.next_sequence.is_some()
                    && self.exit_status.is_none()
                    && self.quarantine_reason.is_none()
            }
            GuardianCensusPaneStatus::ExitedUnclaimed => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.exit_status.is_some()
                    && self.quarantine_reason.is_none()
            }
            GuardianCensusPaneStatus::ClosedTerminal => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.pending_input_effect.is_none()
                    && self.indeterminate_checkpoint_effect.is_none()
                    && self.quarantine_reason.is_none()
            }
            GuardianCensusPaneStatus::Quarantined => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.pending_input_effect.is_none()
                    && self.indeterminate_checkpoint_effect.is_none()
                    && self.quarantine_reason.is_some()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(GuardianProtocolError::InvalidReplyPayload)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianQuarantineReason {
    GenerationExhausted,
    SequenceExhausted,
    EffectOutcomeIndeterminate,
}

impl GuardianCensusPaneStatus {
    const fn to_wire(self) -> u8 {
        match self {
            Self::LiveUnclaimed => 1,
            Self::LiveClaimed => 2,
            Self::ExitedUnclaimed => 3,
            Self::ClosedTerminal => 4,
            Self::Quarantined => 5,
        }
    }

    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            1 => Ok(Self::LiveUnclaimed),
            2 => Ok(Self::LiveClaimed),
            3 => Ok(Self::ExitedUnclaimed),
            4 => Ok(Self::ClosedTerminal),
            5 => Ok(Self::Quarantined),
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
    }
}

impl GuardianQuarantineReason {
    const fn to_wire(self) -> u8 {
        match self {
            Self::GenerationExhausted => 1,
            Self::SequenceExhausted => 2,
            Self::EffectOutcomeIndeterminate => 3,
        }
    }

    fn from_wire(value: u8) -> Result<Option<Self>, GuardianProtocolError> {
        match value {
            0 => Ok(None),
            1 => Ok(Some(Self::GenerationExhausted)),
            2 => Ok(Some(Self::SequenceExhausted)),
            3 => Ok(Some(Self::EffectOutcomeIndeterminate)),
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GuardianProtocolError {
    #[error("guardian frame is shorter than the authenticated protocol envelope")]
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
    #[error("guardian input disposition is invalid for the authenticated input length")]
    InvalidInputDisposition,
    #[error("guardian census page has an invalid encoding or exceeds its entry/byte cap")]
    InvalidCensusPage,
    #[error("guardian census cursor {cursor} exceeds pane count {pane_count}")]
    InvalidCensusCursor { cursor: u64, pane_count: u64 },
    #[error("guardian census snapshot {0} is unavailable or has rotated")]
    CensusSnapshotNotFound(Uuid),
    #[error("guardian census snapshot UUID was reused by a different mux incarnation")]
    CensusSnapshotIdentityConflict,
    #[error("guardian typed reply payload is malformed or violates its operation schema")]
    InvalidReplyPayload,
    #[error("guardian reply variant does not match operation {operation:?}")]
    ReplyOperationMismatch { operation: GuardianOperation },
    #[error("guardian correlated response is not a success reply")]
    NonSuccessResponse,
    #[error("guardian rejection payload is malformed or disagrees with its response status")]
    InvalidRejectionPayload,
    #[error("guardian operation payload is malformed or violates its frozen schema")]
    InvalidOperationPayload,
    #[error("guardian checkpoint intent is malformed, unsupported, or has an absent identity")]
    InvalidCheckpointIntent,
    #[error("guardian checkpoint effects require the typed durable-publication transaction")]
    CheckpointRequiresTypedTransaction,
    #[error("guardian checkpoint publication outcome is indeterminate and cannot be retried")]
    CheckpointOutcomeIndeterminate,
    #[error("guardian checkpoint acknowledgement does not match the pending publication identity")]
    CheckpointIdentityMismatch,
}

impl GuardianRejectionCode {
    #[must_use]
    pub const fn from_protocol_error(error: &GuardianProtocolError) -> Self {
        match error {
            GuardianProtocolError::GuardianIncarnationMismatch => {
                Self::GuardianIncarnationMismatch
            }
            GuardianProtocolError::PaneNotFound(_) => Self::PaneNotFound,
            GuardianProtocolError::PaneAlreadyExists(_) => Self::PaneAlreadyExists,
            GuardianProtocolError::RequestIdentityConflict => Self::RequestIdentityConflict,
            GuardianProtocolError::EffectIdentityConflict => Self::EffectIdentityConflict,
            GuardianProtocolError::PaneTerminal => Self::PaneTerminal,
            GuardianProtocolError::ClaimGenerationMismatch { .. } => {
                Self::ClaimGenerationMismatch
            }
            GuardianProtocolError::StaleLease => Self::StaleLease,
            GuardianProtocolError::RepeatedSequence { .. } => Self::RepeatedSequence,
            GuardianProtocolError::SequenceGap { .. } => Self::SequenceGap,
            GuardianProtocolError::GenerationExhausted => Self::GenerationExhausted,
            GuardianProtocolError::SequenceExhausted => Self::SequenceExhausted,
            GuardianProtocolError::CapacityExhausted => Self::CapacityExhausted,
            GuardianProtocolError::RequestAliasCapacityExhausted { .. } => {
                Self::RequestAliasCapacityExhausted
            }
            GuardianProtocolError::InputDurabilityPending => Self::InputDurabilityPending,
            GuardianProtocolError::InputDurabilityIdentityMismatch => {
                Self::InputDurabilityIdentityMismatch
            }
            GuardianProtocolError::CensusSnapshotNotFound(_) => Self::CensusSnapshotNotFound,
            GuardianProtocolError::CensusSnapshotIdentityConflict => {
                Self::CensusSnapshotIdentityConflict
            }
            GuardianProtocolError::InvalidCensusCursor { .. } => Self::InvalidCensusCursor,
            GuardianProtocolError::StateInvariantViolation(_) => Self::InternalInvariant,
            GuardianProtocolError::CheckpointOutcomeIndeterminate => {
                Self::CheckpointOutcomeIndeterminate
            }
            GuardianProtocolError::CheckpointIdentityMismatch => {
                Self::CheckpointIdentityMismatch
            }
            GuardianProtocolError::TruncatedFrame
            | GuardianProtocolError::FrameLengthMismatch { .. }
            | GuardianProtocolError::FrameTooLarge
            | GuardianProtocolError::PayloadTooLarge
            | GuardianProtocolError::InvalidMagic
            | GuardianProtocolError::UnsupportedVersion(_)
            | GuardianProtocolError::UnknownOperation(_)
            | GuardianProtocolError::UnknownResponseStatus(_)
            | GuardianProtocolError::ReservedFlags
            | GuardianProtocolError::AuthenticationFailed
            | GuardianProtocolError::ResponseRequestMismatch
            | GuardianProtocolError::SecretInitializationFailed
            | GuardianProtocolError::WeakSecret
            | GuardianProtocolError::PayloadDigestMismatch
            | GuardianProtocolError::ZeroIdentity(_)
            | GuardianProtocolError::InvalidOperationScope { .. }
            | GuardianProtocolError::MissingEffectQueryIdentity
            | GuardianProtocolError::InvalidInputDisposition
            | GuardianProtocolError::InvalidCensusPage
            | GuardianProtocolError::InvalidReplyPayload
            | GuardianProtocolError::ReplyOperationMismatch { .. }
            | GuardianProtocolError::NonSuccessResponse
            | GuardianProtocolError::InvalidRejectionPayload
            | GuardianProtocolError::InvalidOperationPayload
            | GuardianProtocolError::InvalidCheckpointIntent
            | GuardianProtocolError::CheckpointRequiresTypedTransaction => Self::InvalidRequest,
        }
    }
}

/// Explicit outcome of one externally observable guardian runtime operation.
///
/// Callers may report `DefinitelyNotApplied` only when they can prove that no
/// externally visible effect occurred. Every ambiguous error path must use
/// `OutcomeIndeterminate`, which permanently fences the pane and exact effect
/// identity until an operation-specific reconciliation mechanism is added.
pub enum GuardianEffectOutcome<E> {
    Applied,
    DefinitelyNotApplied(E),
    OutcomeIndeterminate,
}

impl<E> GuardianEffectOutcome<E> {
    #[must_use]
    pub fn from_definite_result(result: Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::Applied,
            Err(error) => Self::DefinitelyNotApplied(error),
        }
    }
}

pub enum GuardianEffectTransactionError<E> {
    Protocol(GuardianProtocolError),
    Effect(E),
    OutcomeIndeterminate(GuardianReply),
}

impl<E> std::fmt::Debug for GuardianEffectTransactionError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => formatter.debug_tuple("Protocol").field(error).finish(),
            Self::Effect(_) => formatter.debug_tuple("Effect").field(&"[REDACTED]").finish(),
            Self::OutcomeIndeterminate(_) => formatter.write_str("OutcomeIndeterminate"),
        }
    }
}

impl<E> From<GuardianProtocolError> for GuardianEffectTransactionError<E> {
    fn from(error: GuardianProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl<E> std::fmt::Display for GuardianEffectTransactionError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => std::fmt::Display::fmt(error, formatter),
            Self::Effect(_) => formatter.write_str("guardian runtime effect failed"),
            Self::OutcomeIndeterminate(_) => {
                formatter.write_str("guardian runtime effect outcome is indeterminate")
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for GuardianEffectTransactionError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Effect(_) | Self::OutcomeIndeterminate(_) => None,
        }
    }
}

#[derive(Clone, Eq, PartialEq, Hash)]
struct EffectFingerprint {
    operation: GuardianOperation,
    pane_id: Uuid,
    mux_incarnation: Uuid,
    lease_generation: u64,
    lease_sequence: u64,
    payload_bytes: u32,
    payload_sha256: [u8; 32],
}

impl EffectFingerprint {
    fn from_authenticated_request(
        request: &AuthenticatedGuardianRequest,
    ) -> Result<Self, GuardianProtocolError> {
        let pane_id = request
            .header
            .pane_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;
        Ok(Self {
            operation: request.header.operation,
            pane_id,
            mux_incarnation: request.header.mux_incarnation,
            lease_generation: request.header.lease_generation,
            lease_sequence: request.header.lease_sequence,
            payload_bytes: u32::try_from(request.payload.len())
                .map_err(|_| GuardianProtocolError::PayloadTooLarge)?,
            payload_sha256: request.header.payload_sha256,
        })
    }
}
impl std::fmt::Debug for EffectFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectFingerprint")
            .field("operation", &self.operation)
            .field("pane_id", &self.pane_id)
            .field("mux_incarnation", &self.mux_incarnation)
            .field("lease_generation", &self.lease_generation)
            .field("lease_sequence", &self.lease_sequence)
            .field("payload_bytes", &self.payload_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredEffectState {
    Applied,
    OutcomeIndeterminate,
    Input(InputEffectState),
    Checkpoint {
        disposition: GuardianCheckpointDisposition,
        identity: GuardianCheckpointEffectIdentity,
    },
}

impl StoredEffectState {
    const fn is_pending(&self) -> bool {
        matches!(
            self,
            Self::OutcomeIndeterminate
                | Self::Input(InputEffectState::AcceptedNotDurable)
                | Self::Checkpoint {
                    disposition: GuardianCheckpointDisposition::OutcomeIndeterminate,
                    ..
                }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredEffect {
    fingerprint: EffectFingerprint,
    reply: GuardianReply,
    state: StoredEffectState,
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

#[derive(Debug, Default)]
struct ReceiptCapacityPlan {
    request_queue_pops: usize,
    request_ids: Vec<Uuid>,
    effect_queue_pops: usize,
    effect_ids: Vec<Uuid>,
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, PartialEq))]
pub struct GuardianProtocolState {
    incarnation: Uuid,
    panes: BTreeMap<Uuid, GuardianPaneState>,
    census_snapshots: HashMap<Uuid, GuardianCensusSnapshot>,
    census_snapshot_order: VecDeque<Uuid>,
    next_census_snapshot_sequence: u128,
    requests: HashMap<Uuid, StoredRequest>,
    effects: HashMap<Uuid, StoredEffect>,
    effect_request_ids: HashMap<Uuid, HashSet<Uuid>>,
    indeterminate_checkpoints_by_pane: HashMap<Uuid, Uuid>,
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
            next_census_snapshot_sequence: 1,
            requests: HashMap::new(),
            effects: HashMap::new(),
            effect_request_ids: HashMap::new(),
            indeterminate_checkpoints_by_pane: HashMap::new(),
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

    /// Return the authenticated diagnostic receipt for an exact effect whose
    /// callback outcome is already retained as indeterminate.
    ///
    /// This read-only path never invokes the callback, advances protocol state,
    /// or installs a request alias. It exists so a quarantined runtime can
    /// answer an exact identity retry without either misclassifying the effect
    /// as a terminal rejection or admitting another external mutation.
    pub fn indeterminate_effect_reply(
        &self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<Option<GuardianReply>, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch);
        }
        if !request.header.operation.supports_generic_effect_indeterminate() {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            });
        }
        let fingerprint = EffectFingerprint::from_authenticated_request(request)?;
        let effect_id = request
            .header
            .effect_id
            .ok_or(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            })?;

        let intended_reply = if let Some(stored_request) =
            self.requests.get(&request.header.request_id)
        {
            if stored_request.fingerprint != fingerprint
                || stored_request.effect_id != effect_id
            {
                return Err(GuardianProtocolError::RequestIdentityConflict);
            }
            let stored_effect = self.effects.get(&effect_id).ok_or(
                GuardianProtocolError::StateInvariantViolation(
                    "indeterminate-request-effect-reverse-index",
                ),
            )?;
            if stored_effect.fingerprint != fingerprint
                || stored_effect.reply != stored_request.reply
            {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "indeterminate-request-effect-identity",
                ));
            }
            if stored_effect.state != StoredEffectState::OutcomeIndeterminate {
                return Ok(None);
            }
            &stored_request.reply
        } else if let Some(stored_effect) = self.effects.get(&effect_id) {
            if stored_effect.fingerprint != fingerprint {
                return Err(GuardianProtocolError::EffectIdentityConflict);
            }
            if stored_effect.state != StoredEffectState::OutcomeIndeterminate {
                return Ok(None);
            }
            &stored_effect.reply
        } else {
            return Ok(None);
        };

        if !matches!(
            self.panes.get(&fingerprint.pane_id),
            Some(GuardianPaneState::Quarantined {
                reason: GuardianQuarantineReason::EffectOutcomeIndeterminate,
                ..
            })
        ) {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "indeterminate-effect-without-pane-quarantine",
            ));
        }
        GuardianReply::effect_outcome_indeterminate(request, intended_reply).map(Some)
    }

    /// Retire every unambiguous live lease owned by one disconnected mux.
    ///
    /// The transport must call this only after it has proved that no
    /// authenticated connection for `mux_incarnation` remains. The exact
    /// incarnation fence makes a delayed disconnect notification harmless
    /// after a successor has claimed the pane. A pane with an
    /// `AcceptedNotDurable` input or indeterminate checkpoint remains claimed
    /// and blocks takeover until its exact identity is reconciled; the caller
    /// invokes this method again afterward.
    pub fn retire_disconnected_mux_leases(
        &mut self,
        mux_incarnation: Uuid,
    ) -> Result<GuardianMuxLeaseRetirement, GuardianProtocolError> {
        require_nonzero(mux_incarnation, "mux incarnation")?;
        let mut result = GuardianMuxLeaseRetirement::default();
        for (pane_id, state) in &mut self.panes {
            let GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation: owner,
                pending_input_effect,
                ..
            } = state
            else {
                continue;
            };
            if *owner != mux_incarnation {
                continue;
            }
            if pending_input_effect.is_some() {
                result.pending_input_panes += 1;
                continue;
            }
            if self
                .indeterminate_checkpoints_by_pane
                .contains_key(pane_id)
            {
                result.indeterminate_checkpoint_panes += 1;
                continue;
            }
            let generation = *generation;
            *state = GuardianPaneState::LiveUnclaimed { generation };
            result.retired_panes += 1;
        }
        Ok(result)
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

    /// Apply an authenticated operation that cannot create or mutate a runtime effect.
    ///
    /// Effect-producing requests must use the generic or operation-specific
    /// transactional surface. Keeping observation separate prevents a transport
    /// from advancing a lease or recording a spawn before the corresponding
    /// PTY/process operation has actually succeeded.
    pub fn apply_observation(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.operation != GuardianOperation::Hello
            && request.header.guardian_incarnation != self.incarnation
        {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch);
        }

        match request.header.operation {
            GuardianOperation::Hello => Ok(GuardianReply::Hello {
                guardian_incarnation: self.incarnation,
            }),
            GuardianOperation::Census => {
                let page = GuardianCensusPageRequest::decode(&request.payload)?;
                self.census(page, request.header.mux_incarnation)
            }
            GuardianOperation::Attach => self.attach(request),
            GuardianOperation::Replay => self.replay(request),
            GuardianOperation::QueryInputEffect => self.query_input_effect(request),
            operation => Err(GuardianProtocolError::InvalidOperationScope { operation }),
        }
    }

    /// Fence, execute, and commit one effect-producing request.
    ///
    /// The callback is invoked only for a new effect identity, after authentication,
    /// generation, sequence, capacity, and idempotency validation. Exact request/effect
    /// replays return their original receipt without invoking it. A successful pane transition
    /// and its new receipts are committed only after the callback reports `Applied`. Exhausted
    /// generation/sequence counters are the deliberate exception: preflight rejects the effect
    /// and terminally quarantines the pane so wrapped authority can never be revived.
    ///
    /// `DefinitelyNotApplied` restores the pre-effect pane state and leaves the
    /// identity retryable. `OutcomeIndeterminate` and recovered callback panics
    /// retain an exact receipt plus a quarantined pane fence, so retries never
    /// invoke the callback. Input is excluded from this generic surface because
    /// it has its own durable typed transaction.
    pub fn apply_effect_transactionally<E>(
        &mut self,
        request: &AuthenticatedGuardianRequest,
        perform_effect: impl FnOnce(&GuardianReply) -> GuardianEffectOutcome<E>,
    ) -> Result<GuardianReply, GuardianEffectTransactionError<E>> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch.into());
        }
        if request.header.operation == GuardianOperation::Checkpoint {
            return Err(GuardianProtocolError::CheckpointRequiresTypedTransaction.into());
        }
        if request.header.operation == GuardianOperation::Input {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: GuardianOperation::Input,
            }
            .into());
        }
        if !request.header.operation.creates_effect() {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            }
            .into());
        }
        self.apply_effect_transaction_inner(request, perform_effect)
    }

    /// Admit one input only through the journal-owned typed transaction.
    ///
    /// This crate-visible seam deliberately accepts only a callback whose
    /// error means that no PTY write was attempted. The sibling input-journal
    /// module uses it to synchronize both the intent and
    /// `AcceptedNotDurable` records before it allows a non-cloneable write
    /// permit to escape. Keeping this method crate-visible prevents external
    /// callers from treating ordinary effect admission as input authority.
    pub(crate) fn apply_input_effect_transactionally<E>(
        &mut self,
        request: &AuthenticatedGuardianRequest,
        prepare_durable_input: impl FnOnce(&GuardianReply) -> Result<(), E>,
    ) -> Result<GuardianReply, GuardianEffectTransactionError<E>> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch.into());
        }
        if request.header.operation != GuardianOperation::Input {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            }
            .into());
        }
        self.apply_effect_transaction_inner(request, |reply| {
            match prepare_durable_input(reply) {
                Ok(()) => GuardianEffectOutcome::Applied,
                Err(error) => GuardianEffectOutcome::DefinitelyNotApplied(error),
            }
        })
    }

    /// Publish one checkpoint under a typed, exact, non-retryable lifecycle.
    ///
    /// The caller must complete every fallible step that can *prove* no
    /// publication was attempted before entering this method. The callback is
    /// the publication boundary: `Ok(())` means the exact checkpoint and output
    /// boundary are durably committed. Any returned error or recovered panic is
    /// conservatively recorded as `OutcomeIndeterminate`; its value is disposed
    /// inside the audited recoverable-panic boundary and never reaches Debug,
    /// Display, or the wire. Exact request/effect replays return the retained
    /// receipt without invoking the callback. A different mutation cannot pass
    /// the pane fence while an indeterminate publication awaits reconciliation.
    // Intentionally private until a guardian-service dispatcher owns the
    // durable publisher and exposes a deliberately typed integration seam.
    #[allow(dead_code)]
    fn apply_checkpoint_transactionally<E>(
        &mut self,
        request: &AuthenticatedGuardianRequest,
        publish: impl FnOnce(&GuardianCheckpointPending) -> Result<(), E>,
    ) -> Result<GuardianCheckpointReceipt, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch);
        }
        if request.header.operation != GuardianOperation::Checkpoint {
            return Err(GuardianProtocolError::CheckpointRequiresTypedTransaction);
        }

        let identity = GuardianCheckpointEffectIdentity::from_authenticated_request(request)?;
        let fingerprint = EffectFingerprint {
            operation: GuardianOperation::Checkpoint,
            pane_id: identity.pane_id,
            mux_incarnation: identity.mux_incarnation,
            lease_generation: identity.generation,
            lease_sequence: identity.sequence,
            payload_bytes: u32::try_from(request.payload.len())
                .map_err(|_| GuardianProtocolError::PayloadTooLarge)?,
            payload_sha256: request.header.payload_sha256,
        };

        if let Some(stored) = self.requests.get(&identity.request_id) {
            if stored.fingerprint == fingerprint && stored.effect_id == identity.effect_id {
                return Self::checkpoint_receipt_from_reply(&stored.reply);
            }
            return Err(GuardianProtocolError::RequestIdentityConflict);
        }
        if let Some(stored) = self.effects.get(&identity.effect_id) {
            if stored.fingerprint != fingerprint {
                return Err(GuardianProtocolError::EffectIdentityConflict);
            }
            let receipt = Self::checkpoint_receipt_from_reply(&stored.reply)?;
            let disposition_is_pending = stored.state.is_pending();
            if !matches!(stored.state, StoredEffectState::Checkpoint { .. }) {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-effect-state-kind",
                ));
            }
            if disposition_is_pending
                && self
                    .effect_request_ids
                    .get(&identity.effect_id)
                    .map_or(0, HashSet::len)
                    >= GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT
            {
                return Err(GuardianProtocolError::RequestAliasCapacityExhausted {
                    effect_id: identity.effect_id,
                    max_aliases: GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT,
                });
            }
            let capacity = self.plan_receipt_capacity(true, false)?;
            self.requests
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            self.effect_request_ids
                .get_mut(&identity.effect_id)
                .ok_or(GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-effect-request-alias-set",
                ))?
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            if !disposition_is_pending {
                self.transient_request_order
                    .try_reserve(1)
                    .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            }
            self.commit_receipt_capacity(capacity);
            self.requests.insert(
                identity.request_id,
                StoredRequest {
                    fingerprint,
                    effect_id: identity.effect_id,
                    reply: GuardianReply::CheckpointReceipt(receipt),
                },
            );
            self.effect_request_ids
                .get_mut(&identity.effect_id)
                .expect("checkpoint alias set exists after capacity preflight")
                .insert(identity.request_id);
            if !disposition_is_pending {
                self.transient_request_order.push_back(identity.request_id);
            }
            return Ok(receipt);
        }

        let (sequence, next_pane_state) = self.plan_exact_sequence(identity.pane_id, request)?;
        if sequence != identity.sequence {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-planned-sequence-identity",
            ));
        }
        let capacity = self.plan_receipt_capacity(true, true)?;
        self.requests
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.effects
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.effect_request_ids
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.indeterminate_checkpoints_by_pane
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.transient_request_order
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.transient_effect_order
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        let mut request_ids = HashSet::new();
        request_ids
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        request_ids.insert(identity.request_id);

        let pending = GuardianCheckpointPending { identity };
        let publication = catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| publish(&pending)),
        );
        let disposition = match publication {
            Ok(Ok(())) => GuardianCheckpointDisposition::Committed,
            Ok(Err(error)) => {
                let _ = catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| drop(error)),
                );
                GuardianCheckpointDisposition::OutcomeIndeterminate
            }
            Err(_) => GuardianCheckpointDisposition::OutcomeIndeterminate,
        };
        let receipt = GuardianCheckpointReceipt::from_identity(identity, disposition);
        let reply = GuardianReply::CheckpointReceipt(receipt);

        // Every allocation was reserved before publication. From this point
        // forward, the exact receipt and sequence fence are installed without
        // a retryable branch.
        self.commit_receipt_capacity(capacity);
        self.panes.insert(identity.pane_id, next_pane_state);
        self.effects.insert(
            identity.effect_id,
            StoredEffect {
                fingerprint: fingerprint.clone(),
                reply: reply.clone(),
                state: StoredEffectState::Checkpoint {
                    disposition,
                    identity,
                },
            },
        );
        self.requests.insert(
            identity.request_id,
            StoredRequest {
                fingerprint,
                effect_id: identity.effect_id,
                reply,
            },
        );
        self.effect_request_ids
            .insert(identity.effect_id, request_ids);
        if disposition == GuardianCheckpointDisposition::Committed {
            self.transient_request_order.push_back(identity.request_id);
            self.transient_effect_order.push_back(identity.effect_id);
        } else {
            self.indeterminate_checkpoints_by_pane
                .insert(identity.pane_id, identity.effect_id);
        }
        Ok(receipt)
    }

    /// Reconcile a previously indeterminate publication as durably committed.
    /// The exact originating request nonce, effect nonce, generation, sequence,
    /// checkpoint digest, and output-boundary digest must all match.
    #[allow(dead_code)]
    fn mark_checkpoint_committed(
        &mut self,
        identity: GuardianCheckpointEffectIdentity,
    ) -> Result<GuardianCheckpointReceipt, GuardianProtocolError> {
        let stored = self
            .effects
            .get(&identity.effect_id)
            .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?;
        let StoredEffectState::Checkpoint {
            disposition,
            identity: stored_identity,
        } = stored.state
        else {
            return Err(GuardianProtocolError::CheckpointIdentityMismatch);
        };
        if stored_identity != identity {
            return Err(GuardianProtocolError::CheckpointIdentityMismatch);
        }
        if disposition == GuardianCheckpointDisposition::Committed {
            let receipt = Self::checkpoint_receipt_from_reply(&stored.reply)?;
            if receipt.disposition == GuardianCheckpointDisposition::Committed
                && receipt.matches_identity(identity)
                && !self
                    .indeterminate_checkpoints_by_pane
                    .contains_key(&identity.pane_id)
            {
                return Ok(receipt);
            }
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-committed-state",
            ));
        }
        if self
            .indeterminate_checkpoints_by_pane
            .get(&identity.pane_id)
            .copied()
            != Some(identity.effect_id)
        {
            return Err(GuardianProtocolError::CheckpointIdentityMismatch);
        }
        let expected_next_sequence = identity
            .sequence
            .checked_add(1)
            .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?;
        let pane_fence_matches = match self.panes.get(&identity.pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                pending_input_effect,
            }) => {
                *generation == identity.generation
                    && *mux_incarnation == identity.mux_incarnation
                    && *next_sequence == expected_next_sequence
                    && pending_input_effect.is_none()
            }
            Some(GuardianPaneState::ExitedUnclaimed {
                generation,
                pending_input_effect,
                ..
            }) => *generation == identity.generation && pending_input_effect.is_none(),
            _ => false,
        };
        if !pane_fence_matches {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-pane-fence",
            ));
        }

        let pending_reply = stored.reply.clone();
        let pending_receipt = Self::checkpoint_receipt_from_reply(&pending_reply)?;
        if pending_receipt.disposition
            != GuardianCheckpointDisposition::OutcomeIndeterminate
            || !pending_receipt.matches_identity(identity)
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-pending-reply",
            ));
        }
        let fingerprint = stored.fingerprint.clone();
        let request_id_set = self
            .effect_request_ids
            .get(&identity.effect_id)
            .ok_or(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-reverse-index",
            ))?;
        if request_id_set.is_empty() {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-empty-reverse-index",
            ));
        }
        if self.requests.iter().any(|(request_id, request)| {
            request.effect_id == identity.effect_id && !request_id_set.contains(request_id)
        }) {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-incomplete-reverse-index",
            ));
        }
        let mut request_ids = Vec::new();
        request_ids
            .try_reserve(request_id_set.len())
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        request_ids.extend(request_id_set.iter().copied());
        request_ids.sort_unstable();
        for request_id in &request_ids {
            let request = self.requests.get(request_id).ok_or(
                GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-reconciliation-request-alias",
                ),
            )?;
            if request.effect_id != identity.effect_id
                || request.fingerprint != fingerprint
                || request.reply != pending_reply
            {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-reconciliation-request-identity",
                ));
            }
        }
        if self.transient_effect_order.contains(&identity.effect_id)
            || request_ids
                .iter()
                .any(|request_id| self.transient_request_order.contains(request_id))
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-pending-fifo",
            ));
        }
        self.transient_request_order
            .try_reserve(request_ids.len())
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.transient_effect_order
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;

        let receipt = GuardianCheckpointReceipt::from_identity(
            identity,
            GuardianCheckpointDisposition::Committed,
        );
        let reply = GuardianReply::CheckpointReceipt(receipt);
        let stored = self
            .effects
            .get_mut(&identity.effect_id)
            .expect("checkpoint effect exists after reconciliation preflight");
        stored.reply = reply.clone();
        stored.state = StoredEffectState::Checkpoint {
            disposition: GuardianCheckpointDisposition::Committed,
            identity,
        };
        for request_id in &request_ids {
            let request = self
                .requests
                .get_mut(request_id)
                .expect("checkpoint request alias exists after reconciliation preflight");
            request.reply = reply.clone();
        }
        let removed_barrier = self
            .indeterminate_checkpoints_by_pane
            .remove(&identity.pane_id);
        debug_assert_eq!(removed_barrier, Some(identity.effect_id));
        self.transient_request_order.extend(request_ids);
        self.transient_effect_order.push_back(identity.effect_id);
        Ok(receipt)
    }

    /// Reconcile an indeterminate attempt only after the publication owner has
    /// proved that the exact artifact was definitely not published. The
    /// consumed sequence is rolled back only while the exact original claimed
    /// lease remains otherwise untouched; terminal panes stay terminal.
    #[allow(dead_code)]
    fn mark_checkpoint_definitely_not_published(
        &mut self,
        identity: GuardianCheckpointEffectIdentity,
    ) -> Result<(), GuardianProtocolError> {
        let stored = self
            .effects
            .get(&identity.effect_id)
            .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?;
        if !matches!(
            stored.state,
            StoredEffectState::Checkpoint {
                disposition: GuardianCheckpointDisposition::OutcomeIndeterminate,
                identity: stored_identity,
            } if stored_identity == identity
        ) || self
            .indeterminate_checkpoints_by_pane
            .get(&identity.pane_id)
            .copied()
            != Some(identity.effect_id)
        {
            return Err(GuardianProtocolError::CheckpointIdentityMismatch);
        }

        let pending_reply = stored.reply.clone();
        let pending_receipt = Self::checkpoint_receipt_from_reply(&pending_reply)?;
        if pending_receipt.disposition
            != GuardianCheckpointDisposition::OutcomeIndeterminate
            || !pending_receipt.matches_identity(identity)
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-pending-reply",
            ));
        }
        let fingerprint = stored.fingerprint.clone();
        let expected_next_sequence = identity
            .sequence
            .checked_add(1)
            .ok_or(GuardianProtocolError::CheckpointIdentityMismatch)?;
        let rollback_live_sequence = match self.panes.get(&identity.pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                pending_input_effect,
            }) if *generation == identity.generation
                && *mux_incarnation == identity.mux_incarnation
                && *next_sequence == expected_next_sequence
                && pending_input_effect.is_none() =>
            {
                true
            }
            Some(GuardianPaneState::ExitedUnclaimed {
                generation,
                pending_input_effect,
                ..
            }) if *generation == identity.generation && pending_input_effect.is_none() => false,
            _ => return Err(GuardianProtocolError::CheckpointIdentityMismatch),
        };

        let request_ids = self
            .effect_request_ids
            .get(&identity.effect_id)
            .ok_or(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-reverse-index",
            ))?;
        if request_ids.is_empty() {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-empty-reverse-index",
            ));
        }
        if self.requests.iter().any(|(request_id, request)| {
            request.effect_id == identity.effect_id && !request_ids.contains(request_id)
        }) {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-incomplete-reverse-index",
            ));
        }
        for request_id in request_ids {
            let request = self.requests.get(request_id).ok_or(
                GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-definite-not-published-request-alias",
                ),
            )?;
            if request.effect_id != identity.effect_id
                || request.fingerprint != fingerprint
                || request.reply != pending_reply
            {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "checkpoint-definite-not-published-request-identity",
                ));
            }
        }
        if self.transient_effect_order.contains(&identity.effect_id)
            || request_ids
                .iter()
                .any(|request_id| self.transient_request_order.contains(request_id))
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-pending-fifo",
            ));
        }

        if rollback_live_sequence {
            let GuardianPaneState::LiveClaimed { next_sequence, .. } = self
                .panes
                .get_mut(&identity.pane_id)
                .expect("checkpoint pane exists after definite-failure preflight")
            else {
                unreachable!("checkpoint pane shape was checked before definite-failure rollback");
            };
            *next_sequence = identity.sequence;
        }
        let request_ids = self
            .effect_request_ids
            .remove(&identity.effect_id)
            .expect("checkpoint reverse index exists after definite-failure preflight");
        for request_id in request_ids {
            let removed = self.requests.remove(&request_id);
            debug_assert!(removed.is_some());
        }
        let removed_effect = self.effects.remove(&identity.effect_id);
        debug_assert!(removed_effect.is_some());
        let removed_barrier = self
            .indeterminate_checkpoints_by_pane
            .remove(&identity.pane_id);
        debug_assert_eq!(removed_barrier, Some(identity.effect_id));
        Ok(())
    }

    fn checkpoint_receipt_from_reply(
        reply: &GuardianReply,
    ) -> Result<GuardianCheckpointReceipt, GuardianProtocolError> {
        let GuardianReply::CheckpointReceipt(receipt) = reply else {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-stored-reply-kind",
            ));
        };
        Ok(*receipt)
    }

    #[cfg(test)]
    fn apply(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if request.header.operation == GuardianOperation::Checkpoint {
            self.apply_checkpoint_transactionally(request, |_| Ok::<(), std::convert::Infallible>(()))
                .map(GuardianReply::CheckpointReceipt)
        } else if request.header.operation == GuardianOperation::Input {
            // Pure protocol fixtures deliberately bypass the descriptor-backed
            // journal. Production callers cannot: the public generic effect
            // surface rejects Input and the typed input transaction owns the
            // only write permit.
            match self.apply_effect_transaction_inner(request, |_| {
                GuardianEffectOutcome::<std::convert::Infallible>::Applied
            }) {
                Ok(reply) => Ok(reply),
                Err(GuardianEffectTransactionError::Protocol(error)) => Err(error),
                Err(GuardianEffectTransactionError::Effect(never)) => match never {},
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(_)) => Err(
                    GuardianProtocolError::StateInvariantViolation(
                        "pure-input-effect-outcome-indeterminate",
                    ),
                ),
            }
        } else if request.header.operation.creates_effect() {
            match self.apply_effect_transactionally(request, |_| {
                GuardianEffectOutcome::<std::convert::Infallible>::Applied
            }) {
                Ok(reply) => Ok(reply),
                Err(GuardianEffectTransactionError::Protocol(error)) => Err(error),
                Err(GuardianEffectTransactionError::Effect(never)) => match never {},
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(_)) => Err(
                    GuardianProtocolError::StateInvariantViolation(
                        "pure-effect-outcome-indeterminate",
                    ),
                ),
            }
        } else {
            self.apply_observation(request)
        }
    }

    // Production completion flows through the opaque terminal permit in
    // guardian_input_journal so the durable disposition/count is supplied
    // exactly once. These crate-visible primitives remain for that sibling
    // module and mutation-sensitive state-machine tests.
    pub(crate) fn mark_input_durable_full(
        &mut self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(identity, InputEffectState::DurableFull)
    }

    pub(crate) fn mark_input_durable_prefix(
        &mut self,
        identity: GuardianInputEffectIdentity,
        applied_bytes: u32,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(
            identity,
            InputEffectState::DurablePrefix { applied_bytes },
        )
    }

    pub(crate) fn mark_input_known_not_applied(
        &mut self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(identity, InputEffectState::KnownNotApplied)
    }

    fn census(
        &mut self,
        page: GuardianCensusPageRequest,
        mux_incarnation: Uuid,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        let snapshot_id = if page.cursor == 0 {
            let next_sequence = self
                .next_census_snapshot_sequence
                .checked_add(1)
                .ok_or(GuardianProtocolError::CapacityExhausted)?;
            let snapshot_id = Uuid::from_u128(self.next_census_snapshot_sequence);
            if snapshot_id.is_nil() || self.census_snapshots.contains_key(&snapshot_id) {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "guardian-census-snapshot-sequence",
                ));
            }
            let entries = self
                .panes
                .iter()
                .map(|(pane_id, state)| {
                    GuardianCensusEntry::from_state(
                        *pane_id,
                        state,
                        self.indeterminate_checkpoints_by_pane
                            .get(pane_id)
                            .copied(),
                    )
                })
                .collect::<Vec<_>>();
            while self.census_snapshots.len() >= GUARDIAN_MAX_CENSUS_SNAPSHOTS {
                let retired = self
                    .census_snapshot_order
                    .pop_front()
                    .ok_or(GuardianProtocolError::CapacityExhausted)?;
                self.census_snapshots.remove(&retired);
            }
            self.census_snapshots.insert(
                snapshot_id,
                GuardianCensusSnapshot {
                    mux_incarnation,
                    entries,
                },
            );
            self.census_snapshot_order.push_back(snapshot_id);
            self.next_census_snapshot_sequence = next_sequence;
            snapshot_id
        } else {
            page.snapshot_id
        };
        if self
            .census_snapshots
            .get(&snapshot_id)
            .is_some_and(|snapshot| snapshot.mux_incarnation != mux_incarnation)
        {
            return Err(GuardianProtocolError::CensusSnapshotIdentityConflict);
        }
        let snapshot = self
            .census_snapshots
            .get(&snapshot_id)
            .ok_or(GuardianProtocolError::CensusSnapshotNotFound(
                snapshot_id,
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
            snapshot_id,
            entries,
            next_cursor,
            total_panes,
        })
    }

    fn transition_pending_input(
        &mut self,
        identity: GuardianInputEffectIdentity,
        target: InputEffectState,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if !matches!(
            target,
            InputEffectState::DurableFull
                | InputEffectState::DurablePrefix { .. }
                | InputEffectState::KnownNotApplied
        ) {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        target.validate_for_input_bytes(identity.input_bytes)?;
        let effect_id = identity.effect_id;
        let stored = self
            .effects
            .get(&effect_id)
            .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?;
        if stored.fingerprint.operation != GuardianOperation::Input
            || stored.fingerprint.pane_id != identity.pane_id
            || stored.fingerprint.mux_incarnation != identity.mux_incarnation
            || stored.fingerprint.lease_generation != identity.generation
            || stored.fingerprint.lease_sequence != identity.sequence
            || stored.fingerprint.payload_bytes != identity.input_bytes
            || stored.fingerprint.payload_sha256 != identity.payload_sha256
        {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        if stored.state == StoredEffectState::Input(target) {
            let reply = GuardianReply::InputReceipt {
                pane_id: identity.pane_id,
                generation: identity.generation,
                sequence: identity.sequence,
                effect_id,
                state: target,
            };
            if stored.reply == reply {
                return Ok(reply);
            }
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-terminal-reply",
            ));
        }
        if stored.state != StoredEffectState::Input(InputEffectState::AcceptedNotDurable) {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        let pane_id = stored.fingerprint.pane_id;
        let generation = stored.fingerprint.lease_generation;
        let sequence = stored.fingerprint.lease_sequence;
        let fingerprint = stored.fingerprint.clone();
        let pending_reply = GuardianReply::InputReceipt {
            pane_id,
            generation,
            sequence,
            effect_id,
            state: InputEffectState::AcceptedNotDurable,
        };
        if stored.reply != pending_reply {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-pending-reply",
            ));
        }
        let expected_next_sequence = sequence.checked_add(1).ok_or(
            GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-sequence-fence",
            ),
        )?;
        let pane_fence_matches = match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed {
                generation: pane_generation,
                mux_incarnation,
                next_sequence,
                pending_input_effect,
            }) => {
                *pane_generation == generation
                    && *mux_incarnation == identity.mux_incarnation
                    && *next_sequence == expected_next_sequence
                    && *pending_input_effect == Some(effect_id)
            }
            Some(GuardianPaneState::ExitedUnclaimed {
                generation: pane_generation,
                pending_input_effect,
                ..
            }) => *pane_generation == generation && *pending_input_effect == Some(effect_id),
            _ => false,
        };
        if !pane_fence_matches {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-pane-fence",
            ));
        }

        let request_id_set = self.effect_request_ids.get(&effect_id).ok_or(
            GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-reverse-index",
            ),
        )?;
        if request_id_set.is_empty() {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-empty-reverse-index",
            ));
        }
        if self.requests.iter().any(|(request_id, request)| {
            request.effect_id == effect_id && !request_id_set.contains(request_id)
        }) {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-incomplete-reverse-index",
            ));
        }
        let mut request_ids = Vec::new();
        request_ids
            .try_reserve(request_id_set.len())
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        request_ids.extend(request_id_set.iter().copied());
        request_ids.sort_unstable();
        for request_id in &request_ids {
            let request = self.requests.get(request_id).ok_or(
                GuardianProtocolError::StateInvariantViolation(
                    "input-reconciliation-request-alias",
                ),
            )?;
            if request.effect_id != effect_id
                || request.fingerprint != fingerprint
                || request.reply != pending_reply
            {
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "input-reconciliation-request-identity",
                ));
            }
        }
        if self.transient_effect_order.contains(&effect_id)
            || request_ids
                .iter()
                .any(|request_id| self.transient_request_order.contains(request_id))
        {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-pending-fifo",
            ));
        }
        self.transient_request_order
            .try_reserve(request_ids.len())
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.transient_effect_order
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;

        let reply = GuardianReply::InputReceipt {
            pane_id,
            generation,
            sequence,
            effect_id,
            state: target,
        };
        match self
            .panes
            .get_mut(&pane_id)
            .expect("input pane exists after reconciliation preflight")
        {
            GuardianPaneState::LiveClaimed {
                pending_input_effect,
                ..
            }
            | GuardianPaneState::ExitedUnclaimed {
                pending_input_effect,
                ..
            } => *pending_input_effect = None,
            _ => unreachable!("input pane shape was checked before reconciliation"),
        }
        let stored = self
            .effects
            .get_mut(&effect_id)
            .expect("input effect exists after reconciliation preflight");
        stored.state = StoredEffectState::Input(target);
        stored.reply = reply.clone();
        for request_id in &request_ids {
            let request = self
                .requests
                .get_mut(request_id)
                .expect("input request alias exists after reconciliation preflight");
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
        let query = GuardianInputEffectQuery::decode(&request.payload)?;
        let state = match self.effects.get(&effect_id) {
            None => self.missing_input_effect_state(pane_id, query.sequence)?,
            Some(stored)
                if stored.fingerprint.pane_id == pane_id
                    && stored.fingerprint.operation == GuardianOperation::Input
                    && stored.fingerprint.mux_incarnation == request.header.mux_incarnation
                    && stored.fingerprint.lease_generation == request.header.lease_generation
                    && stored.fingerprint.lease_sequence == query.sequence
                    && stored.fingerprint.payload_bytes == query.input_bytes
                    && stored.fingerprint.payload_sha256 == query.payload_sha256 =>
            {
                let StoredEffectState::Input(state) = stored.state else {
                    return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
                };
                state.validate_for_input_bytes(query.input_bytes)?;
                state
            }
            Some(_) => {
                return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
            }
        };
        Ok(GuardianReply::InputEffect { effect_id, state })
    }

    fn missing_input_effect_state(
        &self,
        pane_id: Uuid,
        sequence: u64,
    ) -> Result<InputEffectState, GuardianProtocolError> {
        match self.panes.get(&pane_id) {
            Some(GuardianPaneState::LiveClaimed { next_sequence, .. }) => {
                if sequence < *next_sequence {
                    Ok(InputEffectState::DispositionUnavailable)
                } else {
                    Ok(InputEffectState::NotSeen)
                }
            }
            Some(GuardianPaneState::LiveUnclaimed { generation: 0 })
            | Some(GuardianPaneState::ExitedUnclaimed { generation: 0, .. })
            | Some(GuardianPaneState::ClosedTerminal { generation: 0, .. }) => {
                Ok(InputEffectState::NotSeen)
            }
            Some(
                GuardianPaneState::LiveUnclaimed { .. }
                | GuardianPaneState::ExitedUnclaimed { .. }
                | GuardianPaneState::ClosedTerminal { .. }
                | GuardianPaneState::Quarantined { .. },
            ) => Ok(InputEffectState::DispositionUnavailable),
            None => Err(GuardianProtocolError::PaneNotFound(pane_id)),
        }
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

    fn apply_effect_transaction_inner<E>(
        &mut self,
        request: &AuthenticatedGuardianRequest,
        perform_effect: impl FnOnce(&GuardianReply) -> GuardianEffectOutcome<E>,
    ) -> Result<GuardianReply, GuardianEffectTransactionError<E>> {
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
        let fingerprint = EffectFingerprint::from_authenticated_request(request)?;

        if let Some(stored) = self.requests.get(&request.header.request_id) {
            if stored.fingerprint == fingerprint && stored.effect_id == effect_id {
                let effect = self.effects.get(&effect_id).ok_or(
                    GuardianProtocolError::StateInvariantViolation(
                        "effect-request-replay-reverse-index",
                    ),
                )?;
                if effect.state == StoredEffectState::OutcomeIndeterminate {
                    return Err(GuardianEffectTransactionError::OutcomeIndeterminate(
                        stored.reply.clone(),
                    ));
                }
                return Ok(stored.reply.clone());
            }
            return Err(GuardianProtocolError::RequestIdentityConflict.into());
        }
        if let Some(stored) = self.effects.get(&effect_id) {
            if stored.fingerprint == fingerprint {
                let reply = stored.reply.clone();
                let disposition_is_pending = stored.state.is_pending();
                let outcome_is_indeterminate =
                    stored.state == StoredEffectState::OutcomeIndeterminate;
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
                    }
                    .into());
                }
                let capacity = self.plan_receipt_capacity(true, false)?;
                self.requests
                    .try_reserve(1)
                    .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
                self.effect_request_ids
                    .get_mut(&effect_id)
                    .ok_or(GuardianProtocolError::StateInvariantViolation(
                        "effect-request-alias-set",
                    ))?
                    .try_reserve(1)
                    .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
                if !disposition_is_pending {
                    self.transient_request_order
                        .try_reserve(1)
                        .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
                }
                self.commit_receipt_capacity(capacity);
                self.requests.insert(
                    request.header.request_id,
                    StoredRequest {
                        fingerprint,
                        effect_id,
                        reply: reply.clone(),
                    },
                );
                self.effect_request_ids
                    .get_mut(&effect_id)
                    .expect("effect alias set exists after capacity preflight")
                    .insert(request.header.request_id);
                if !disposition_is_pending {
                    self.transient_request_order
                        .push_back(request.header.request_id);
                }
                if outcome_is_indeterminate {
                    return Err(GuardianEffectTransactionError::OutcomeIndeterminate(reply));
                }
                return Ok(reply);
            }
            return Err(GuardianProtocolError::EffectIdentityConflict.into());
        }

        let (reply, next_pane_state) =
            self.plan_new_effect(pane_id, effect_id, request)?;
        let capacity = self.plan_receipt_capacity(true, true)?;
        self.requests
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.effects
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        self.effect_request_ids
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        if request.header.operation == GuardianOperation::Spawn {
            self.protected_spawn_requests
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            self.protected_spawn_effects
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        } else if request.header.operation != GuardianOperation::Input {
            self.transient_request_order
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            self.transient_effect_order
                .try_reserve(1)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        }
        let mut request_ids = HashSet::new();
        request_ids
            .try_reserve(1)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        request_ids.insert(request.header.request_id);
        let effect_fingerprint = fingerprint.clone();
        let effect_reply = reply.clone();
        let request_reply = reply.clone();

        // BTreeMap cannot reserve a node. Install a conservative quarantine
        // before invoking the callback: this performs the only possibly
        // allocating pane insertion for Spawn and leaves a non-retryable fence
        // if the callback panics. A typed definite failure restores the exact
        // prior state without allocating.
        let prior_pane_state = self.panes.get(&pane_id).cloned();
        let quarantine = GuardianPaneState::Quarantined {
            generation: next_pane_state.generation(),
            reason: GuardianQuarantineReason::EffectOutcomeIndeterminate,
            exit_status: prior_pane_state.as_ref().and_then(|state| match state {
                GuardianPaneState::ExitedUnclaimed { exit_status, .. } => Some(*exit_status),
                GuardianPaneState::ClosedTerminal { exit_status, .. }
                | GuardianPaneState::Quarantined { exit_status, .. } => *exit_status,
                GuardianPaneState::LiveUnclaimed { .. }
                | GuardianPaneState::LiveClaimed { .. } => None,
            }),
        };
        if request.header.operation == GuardianOperation::Spawn {
            if let Some(unexpected_pane_state) = self.panes.insert(pane_id, quarantine) {
                // `plan_new_effect` established that this pane ID was absent,
                // and no external callback has run since that preflight.  If
                // the invariant is ever violated, restore the displaced state
                // before failing closed.  The key is already present, so this
                // replacement does not need to allocate a new BTreeMap node.
                let displaced_quarantine = self.panes.insert(pane_id, unexpected_pane_state);
                debug_assert!(displaced_quarantine.is_some());
                return Err(GuardianProtocolError::StateInvariantViolation(
                    "spawn-pane-appeared-after-preflight",
                )
                .into());
            }
        } else {
            let pane = self.panes.get_mut(&pane_id).ok_or(
                GuardianProtocolError::StateInvariantViolation(
                    "effect-pane-disappeared-after-preflight",
                ),
            )?;
            *pane = quarantine;
        }

        let outcome = match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| perform_effect(&reply)),
        ) {
            Ok(outcome) => outcome,
            Err(_) => GuardianEffectOutcome::OutcomeIndeterminate,
        };
        let outcome_is_indeterminate = match outcome {
            GuardianEffectOutcome::Applied => false,
            GuardianEffectOutcome::OutcomeIndeterminate => true,
            GuardianEffectOutcome::DefinitelyNotApplied(error) => {
                if let Some(prior) = prior_pane_state {
                    *self
                        .panes
                        .get_mut(&pane_id)
                        .expect("existing effect pane remains installed after callback") = prior;
                } else {
                    let removed = self.panes.remove(&pane_id);
                    debug_assert!(removed.is_some());
                }
                return Err(GuardianEffectTransactionError::Effect(error));
            }
        };

        // Every allocation and the conservative pane fence were completed
        // before the callback. From here the exact receipt can be committed
        // without a retryable branch.
        self.commit_receipt_capacity(capacity);
        if !outcome_is_indeterminate {
            *self
                .panes
                .get_mut(&pane_id)
                .expect("effect pane remains installed after callback") = next_pane_state;
        }
        self.effects.insert(
            effect_id,
            StoredEffect {
                fingerprint: effect_fingerprint,
                reply: effect_reply,
                state: if outcome_is_indeterminate {
                    StoredEffectState::OutcomeIndeterminate
                } else if request.header.operation == GuardianOperation::Input {
                    StoredEffectState::Input(InputEffectState::AcceptedNotDurable)
                } else {
                    StoredEffectState::Applied
                },
            },
        );
        self.requests.insert(
            request.header.request_id,
            StoredRequest {
                fingerprint,
                effect_id,
                reply: request_reply,
            },
        );
        self.effect_request_ids.insert(effect_id, request_ids);
        if request.header.operation == GuardianOperation::Spawn {
            self.protected_spawn_requests
                .insert(request.header.request_id);
            self.protected_spawn_effects.insert(effect_id);
        } else if !outcome_is_indeterminate
            && request.header.operation != GuardianOperation::Input
        {
            self.transient_request_order
                .push_back(request.header.request_id);
            self.transient_effect_order.push_back(effect_id);
        }
        if outcome_is_indeterminate {
            Err(GuardianEffectTransactionError::OutcomeIndeterminate(reply))
        } else {
            Ok(reply)
        }
    }

    fn plan_new_effect(
        &mut self,
        pane_id: Uuid,
        effect_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<(GuardianReply, GuardianPaneState), GuardianProtocolError> {
        match request.header.operation {
            GuardianOperation::Spawn => {
                if self.panes.contains_key(&pane_id) {
                    return Err(GuardianProtocolError::PaneAlreadyExists(pane_id));
                }
                if self.panes.len() >= GUARDIAN_MAX_PANES {
                    return Err(GuardianProtocolError::CapacityExhausted);
                }
                Ok((
                    GuardianReply::Spawned {
                        pane_id,
                        generation: 0,
                    },
                    GuardianPaneState::LiveUnclaimed { generation: 0 },
                ))
            }
            GuardianOperation::Claim => {
                let state = self
                    .panes
                    .get(&pane_id)
                    .cloned()
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
                if self
                    .indeterminate_checkpoints_by_pane
                    .contains_key(&pane_id)
                {
                    return Err(GuardianProtocolError::CheckpointOutcomeIndeterminate);
                }
                let Some(generation) = current.checked_add(1) else {
                    self.panes.insert(
                        pane_id,
                        GuardianPaneState::Quarantined {
                            generation: current,
                            reason: GuardianQuarantineReason::GenerationExhausted,
                            exit_status: None,
                        },
                    );
                    return Err(GuardianProtocolError::GenerationExhausted);
                };
                Ok((
                    GuardianReply::Claimed {
                        pane_id,
                        generation,
                        next_sequence: 1,
                    },
                    GuardianPaneState::LiveClaimed {
                        generation,
                        mux_incarnation: request.header.mux_incarnation,
                        next_sequence: 1,
                        pending_input_effect: None,
                    },
                ))
            }
            GuardianOperation::RetireLease => {
                let (sequence, _next_state) = self.plan_exact_sequence(pane_id, request)?;
                let generation = request.header.lease_generation;
                let _ = sequence;
                Ok((
                    GuardianReply::LeaseRetired {
                        pane_id,
                        generation,
                    },
                    GuardianPaneState::LiveUnclaimed { generation },
                ))
            }
            GuardianOperation::Input => {
                let (sequence, mut next_state) = self.plan_exact_sequence(pane_id, request)?;
                let GuardianPaneState::LiveClaimed {
                    pending_input_effect,
                    ..
                } = &mut next_state
                else {
                    return Err(GuardianProtocolError::StateInvariantViolation(
                        "planned-input-claimed-pane",
                    ));
                };
                *pending_input_effect = Some(effect_id);
                Ok((
                    GuardianReply::InputReceipt {
                        pane_id,
                        generation: request.header.lease_generation,
                        sequence,
                        effect_id,
                        state: InputEffectState::AcceptedNotDurable,
                    },
                    next_state,
                ))
            }
            GuardianOperation::Resize
            | GuardianOperation::Signal => {
                let (sequence, next_state) = self.plan_exact_sequence(pane_id, request)?;
                Ok((
                    GuardianReply::MutationApplied {
                        pane_id,
                        generation: request.header.lease_generation,
                        sequence,
                    },
                    next_state,
                ))
            }
            GuardianOperation::Close => {
                if self
                    .indeterminate_checkpoints_by_pane
                    .contains_key(&pane_id)
                {
                    return Err(GuardianProtocolError::CheckpointOutcomeIndeterminate);
                }
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
                    return Ok((
                        GuardianReply::MutationApplied {
                            pane_id,
                            generation: *generation,
                            sequence: 0,
                        },
                        GuardianPaneState::ClosedTerminal {
                            generation: *generation,
                            exit_status: Some(*exit_status),
                        },
                    ));
                }
                let (sequence, _next_state) = self.plan_exact_sequence(pane_id, request)?;
                let generation = request.header.lease_generation;
                Ok((
                    GuardianReply::MutationApplied {
                        pane_id,
                        generation,
                        sequence,
                    },
                    GuardianPaneState::ClosedTerminal {
                        generation,
                        exit_status: None,
                    },
                ))
            }
            _ => Err(GuardianProtocolError::StateInvariantViolation(
                "effect-operation-classification",
            )),
        }
    }

    fn plan_exact_sequence(
        &mut self,
        pane_id: Uuid,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<(u64, GuardianPaneState), GuardianProtocolError> {
        let expected = self.require_current_lease(pane_id, request)?;
        let state = self
            .panes
            .get(&pane_id)
            .cloned()
            .ok_or(GuardianProtocolError::PaneNotFound(pane_id))?;
        if matches!(
            state,
            GuardianPaneState::LiveClaimed {
                pending_input_effect: Some(_),
                ..
            }
        ) {
            return Err(GuardianProtocolError::InputDurabilityPending);
        }
        if self
            .indeterminate_checkpoints_by_pane
            .contains_key(&pane_id)
        {
            return Err(GuardianProtocolError::CheckpointOutcomeIndeterminate);
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
        let Some(next_sequence) = expected.checked_add(1) else {
            self.panes.insert(
                pane_id,
                GuardianPaneState::Quarantined {
                    generation: request.header.lease_generation,
                    reason: GuardianQuarantineReason::SequenceExhausted,
                    exit_status: None,
                },
            );
            return Err(GuardianProtocolError::SequenceExhausted);
        };
        let GuardianPaneState::LiveClaimed {
            generation,
            mux_incarnation,
            pending_input_effect,
            ..
        } = state
        else {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "planned-exact-sequence-current-lease",
            ));
        };
        Ok((
            expected,
            GuardianPaneState::LiveClaimed {
                generation,
                mux_incarnation,
                next_sequence,
                pending_input_effect,
            },
        ))
    }

    fn plan_receipt_capacity(
        &self,
        new_request: bool,
        new_effect: bool,
    ) -> Result<ReceiptCapacityPlan, GuardianProtocolError> {
        let mut plan = ReceiptCapacityPlan::default();
        if new_request && self.requests.len() >= self.receipt_capacity {
            let needed = self.requests.len() - self.receipt_capacity + 1;
            plan.request_ids
                .try_reserve(needed)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            for (index, request_id) in self.transient_request_order.iter().enumerate() {
                if self.requests.contains_key(request_id) && !plan.request_ids.contains(request_id) {
                    plan.request_ids.push(*request_id);
                    if plan.request_ids.len() == needed {
                        plan.request_queue_pops = index + 1;
                        break;
                    }
                }
            }
            if plan.request_ids.len() != needed {
                return Err(GuardianProtocolError::CapacityExhausted);
            }
        }
        if new_effect && self.effects.len() >= self.receipt_capacity {
            let needed = self.effects.len() - self.receipt_capacity + 1;
            plan.effect_ids
                .try_reserve(needed)
                .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
            for (index, effect_id) in self.transient_effect_order.iter().enumerate() {
                if self.effects.contains_key(effect_id) && !plan.effect_ids.contains(effect_id) {
                    plan.effect_ids.push(*effect_id);
                    if plan.effect_ids.len() == needed {
                        plan.effect_queue_pops = index + 1;
                        break;
                    }
                }
            }
            if plan.effect_ids.len() != needed {
                return Err(GuardianProtocolError::CapacityExhausted);
            }
        }
        Ok(plan)
    }

    fn commit_receipt_capacity(&mut self, plan: ReceiptCapacityPlan) {
        for _ in 0..plan.request_queue_pops {
            let _ = self.transient_request_order.pop_front();
        }
        for request_id in plan.request_ids {
            debug_assert!(!self.protected_spawn_requests.contains(&request_id));
            if let Some(request) = self.requests.remove(&request_id) {
                if let Some(request_ids) = self.effect_request_ids.get_mut(&request.effect_id) {
                    request_ids.remove(&request_id);
                }
            }
        }
        for _ in 0..plan.effect_queue_pops {
            let _ = self.transient_effect_order.pop_front();
        }
        for effect_id in plan.effect_ids {
            debug_assert!(!self.protected_spawn_effects.contains(&effect_id));
            self.effects.remove(&effect_id);
            if let Some(request_ids) = self.effect_request_ids.remove(&effect_id) {
                for request_id in request_ids {
                    debug_assert!(!self.protected_spawn_requests.contains(&request_id));
                    self.requests.remove(&request_id);
                    self.transient_request_order
                        .retain(|queued| *queued != request_id);
                }
            }
        }
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

    let authenticated_payload_bytes = read_u32(frame, REQUEST_PAYLOAD_LENGTH_OFFSET)?;
    let payload_len = usize::try_from(authenticated_payload_bytes)
        .map_err(|_| GuardianProtocolError::PayloadTooLarge)?;
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
    Ok(AuthenticatedGuardianRequest {
        envelope: request,
        authenticated_payload_bytes,
    })
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
    if header.operation == GuardianOperation::Hello {
        if !header.guardian_incarnation.is_nil() {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: header.operation,
            });
        }
    } else {
        require_nonzero(header.guardian_incarnation, "guardian incarnation")?;
    }
    require_nonzero(header.mux_incarnation, "mux incarnation")?;
    require_nonzero(header.request_id, "request")?;
    if header.pane_id.is_some_and(|pane_id| pane_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("pane"));
    }
    if header.effect_id.is_some_and(|effect_id| effect_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("effect"));
    }
    validate_operation_scope(
        header.operation,
        header.pane_id,
        header.effect_id,
        header.lease_generation,
        header.lease_sequence,
    )?;
    if matches!(
        header.status,
        GuardianResponseStatus::Success | GuardianResponseStatus::Indeterminate
    ) {
        let reply = GuardianReply::decode_for_operation(header.operation, &response.payload)?;
        if reply.response_status() != header.status {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        reply.require_response_identity(header)?;
    } else {
        GuardianRejectionCode::decode(header.status, &response.payload)?;
    }
    Ok(())
}

fn validate_operation_scope(
    operation: GuardianOperation,
    pane_id: Option<Uuid>,
    effect_id: Option<Uuid>,
    lease_generation: u64,
    lease_sequence: u64,
) -> Result<(), GuardianProtocolError> {
    let pane_required = !matches!(
        operation,
        GuardianOperation::Census
            | GuardianOperation::Hello
            | GuardianOperation::GuardedStop
    );
    let lease_required = operation.requires_lease();
    let effect_required =
        operation.creates_effect() || operation == GuardianOperation::QueryInputEffect;
    let spawn_scope_ok = operation != GuardianOperation::Spawn
        || (lease_generation == 0 && lease_sequence == 0);
    let observation_scope_ok = !matches!(
        operation,
        GuardianOperation::Census | GuardianOperation::Hello
    ) || (pane_id.is_none()
        && effect_id.is_none()
        && lease_generation == 0
        && lease_sequence == 0);
    let guarded_stop_scope_ok = operation != GuardianOperation::GuardedStop
        || (pane_id.is_none()
            && effect_id.is_some()
            && lease_generation == 0
            && lease_sequence == 0);
    let claim_scope_ok = operation != GuardianOperation::Claim || lease_sequence == 0;
    let sequence_scope_ok = operation.uses_mutation_sequence() || lease_sequence == 0;
    if pane_required != pane_id.is_some()
        || effect_required != effect_id.is_some()
        || (!lease_required
            && !matches!(operation, GuardianOperation::Spawn | GuardianOperation::Claim)
            && (lease_generation != 0 || lease_sequence != 0))
        || !spawn_scope_ok
        || !observation_scope_ok
        || !guarded_stop_scope_ok
        || !claim_scope_ok
        || !sequence_scope_ok
    {
        Err(GuardianProtocolError::InvalidOperationScope { operation })
    } else {
        Ok(())
    }
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
    let operation = header.operation;
    if operation == GuardianOperation::Hello {
        if !header.guardian_incarnation.is_nil() {
            return Err(GuardianProtocolError::InvalidOperationScope { operation });
        }
    } else {
        require_nonzero(header.guardian_incarnation, "guardian incarnation")?;
    }
    require_nonzero(header.mux_incarnation, "mux incarnation")?;
    require_nonzero(header.request_id, "request")?;
    if header.pane_id.is_some_and(|pane_id| pane_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("pane"));
    }
    if header.effect_id.is_some_and(|effect_id| effect_id.is_nil()) {
        return Err(GuardianProtocolError::ZeroIdentity("effect"));
    }

    validate_operation_scope(
        operation,
        header.pane_id,
        header.effect_id,
        header.lease_generation,
        header.lease_sequence,
    )?;
    match operation {
        GuardianOperation::Spawn => {
            GuardianSpawnPayload::decode(&request.payload)?;
        }
        GuardianOperation::Census => {
            GuardianCensusPageRequest::decode(&request.payload)?;
        }
        GuardianOperation::Input
            if request.payload.is_empty() || request.payload.len() > GUARDIAN_MAX_INPUT_BYTES =>
        {
            return Err(GuardianProtocolError::InvalidOperationScope { operation });
        }
        GuardianOperation::Resize => {
            GuardianResizePayload::decode(&request.payload)?;
        }
        GuardianOperation::Signal => {
            GuardianSignal::decode(&request.payload)?;
        }
        GuardianOperation::QueryInputEffect => {
            GuardianInputEffectQuery::decode(&request.payload)?;
        }
        GuardianOperation::Checkpoint => {
            GuardianCheckpointIntent::decode(&request.payload)?;
        }
        GuardianOperation::Hello
        | GuardianOperation::GuardedStop
        | GuardianOperation::Claim
        | GuardianOperation::Attach
        | GuardianOperation::Close
        | GuardianOperation::Replay
        | GuardianOperation::RetireLease
            if !request.payload.is_empty() =>
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
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

fn encode_pty_size(buffer: &mut Vec<u8>, size: PtySize) {
    buffer.extend_from_slice(&size.rows.to_be_bytes());
    buffer.extend_from_slice(&size.cols.to_be_bytes());
    buffer.extend_from_slice(&size.pixel_width.to_be_bytes());
    buffer.extend_from_slice(&size.pixel_height.to_be_bytes());
}

fn decode_pty_size(payload: &[u8]) -> Result<PtySize, GuardianProtocolError> {
    if payload.len() != 8 {
        return Err(GuardianProtocolError::InvalidOperationPayload);
    }
    let size = PtySize {
        rows: u16::from_be_bytes([payload[0], payload[1]]),
        cols: u16::from_be_bytes([payload[2], payload[3]]),
        pixel_width: u16::from_be_bytes([payload[4], payload[5]]),
        pixel_height: u16::from_be_bytes([payload[6], payload[7]]),
    };
    Ok(size)
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

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, GuardianProtocolError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(GuardianProtocolError::InvalidReplyPayload)?;
    Ok(i32::from_be_bytes([value[0], value[1], value[2], value[3]]))
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

fn read_required_uuid(bytes: &[u8], offset: usize) -> Result<Uuid, GuardianProtocolError> {
    let value = read_uuid(bytes, offset).map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
    if value.is_nil() {
        Err(GuardianProtocolError::InvalidReplyPayload)
    } else {
        Ok(value)
    }
}

fn require_reply_len(payload: &[u8], expected: usize) -> Result<(), GuardianProtocolError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(GuardianProtocolError::InvalidReplyPayload)
    }
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

    fn input_effect_identity(
        request: &GuardianRequestEnvelope,
    ) -> GuardianInputEffectIdentity {
        GuardianInputEffectIdentity::from_authenticated_request(&authenticate(request))
            .expect("test input request carries an exact authenticated effect identity")
    }

    fn input_effect_query_payload(
        request: &GuardianRequestEnvelope,
    ) -> [u8; INPUT_EFFECT_QUERY_PAYLOAD_BYTES] {
        assert_eq!(request.header.operation, GuardianOperation::Input);
        GuardianInputEffectQuery::new(
            request.header.lease_sequence,
            u32::try_from(request.payload.len()).expect("fixture input length fits u32"),
            request.header.payload_sha256,
        )
        .expect("test input request carries a nonzero mutation sequence")
        .encode()
    }

    fn pty_size(rows: u16, cols: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn resize_payload(rows: u16, cols: u16) -> [u8; RESIZE_PAYLOAD_BYTES] {
        GuardianResizePayload::new(pty_size(rows, cols)).encode()
    }

    fn spawn_payload(command: &str) -> Vec<u8> {
        GuardianSpawnPayload::new(CommandBuilder::new(command), pty_size(24, 80))
            .unwrap()
            .encode()
            .unwrap()
    }

    fn spawn_request(guardian: Uuid, mux: Uuid, pane: Uuid) -> GuardianRequestEnvelope {
        let payload = spawn_payload("bounded-command");
        request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(4),
            Some(pane),
            0,
            0,
            Some(id(5)),
            &payload,
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

    fn checkpoint_intent(
        checkpoint_byte: u8,
        output_boundary_byte: u8,
    ) -> GuardianCheckpointIntent {
        GuardianCheckpointIntent::new(
            GuardianCheckpointIdentityDigest::from_bytes([checkpoint_byte; 32]).unwrap(),
            GuardianOutputBoundaryIdentityDigest::from_bytes([output_boundary_byte; 32]).unwrap(),
        )
    }

    fn checkpoint_request(
        guardian: Uuid,
        mux: Uuid,
        pane: Uuid,
        generation: u64,
        sequence: u64,
        request_byte: u8,
        effect_byte: u8,
        checkpoint_byte: u8,
        output_boundary_byte: u8,
    ) -> GuardianRequestEnvelope {
        let intent = checkpoint_intent(checkpoint_byte, output_boundary_byte).encode();
        request(
            GuardianOperation::Checkpoint,
            guardian,
            mux,
            id(request_byte),
            Some(pane),
            generation,
            sequence,
            Some(id(effect_byte)),
            &intent,
        )
    }

    fn checkpoint_effect_identity(
        request: &GuardianRequestEnvelope,
    ) -> GuardianCheckpointEffectIdentity {
        GuardianCheckpointEffectIdentity::from_authenticated_request(&authenticate(request))
            .expect("test checkpoint carries an exact authenticated publication identity")
    }

    #[test]
    fn effect_payloads_are_typed_bounded_and_content_free_in_debug() {
        let mut command = CommandBuilder::new("fixture-program");
        command.arg("super-secret-argument");
        command.env("FIXTURE_TOKEN", "super-secret-environment");
        let spawn = GuardianSpawnPayload::new(command, pty_size(31, 101)).unwrap();
        let debug = format!("{spawn:?}");
        assert!(!debug.contains("super-secret"));
        let encoded = spawn.encode().unwrap();
        assert!(encoded.len() <= GUARDIAN_MAX_PAYLOAD_BYTES);
        assert_eq!(GuardianSpawnPayload::decode(&encoded).unwrap(), spawn);
        let mut hidden_field = encoded.clone();
        let insertion = hidden_field.len() - 1;
        hidden_field.splice(insertion..insertion, b",\"hidden\":true".iter().copied());
        let hidden_command_bytes = hidden_field.len() - SPAWN_PAYLOAD_FIXED_BYTES;
        hidden_field[12..16].copy_from_slice(
            &u32::try_from(hidden_command_bytes)
                .unwrap()
                .to_be_bytes(),
        );
        assert_eq!(
            GuardianSpawnPayload::decode(&hidden_field),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );
        assert_eq!(
            GuardianSpawnPayload::new(CommandBuilder::new(""), pty_size(24, 80)),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );

        let resize = GuardianResizePayload::new(pty_size(44, 132));
        assert_eq!(GuardianResizePayload::decode(&resize.encode()).unwrap(), resize);
        let zero_geometry = GuardianResizePayload::new(pty_size(0, 0));
        assert_eq!(
            GuardianResizePayload::decode(&zero_geometry.encode()).unwrap(),
            zero_geometry
        );
        assert_eq!(
            GuardianSignal::decode(&GuardianSignal::Terminate.encode()).unwrap(),
            GuardianSignal::Terminate
        );
        let mut unknown_signal = GuardianSignal::Terminate.encode();
        unknown_signal[4] = 2;
        assert_eq!(
            GuardianSignal::decode(&unknown_signal),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );

        let query = GuardianInputEffectQuery::new(7, 19, [0x3c; 32]).unwrap();
        assert!(
            !format!("{query:?}").contains(&format!("{:?}", [0x3c; 32])),
            "query diagnostics must not expose a dictionary-testable input digest"
        );
        assert_eq!(
            GuardianInputEffectQuery::decode(&query.encode()).unwrap(),
            query
        );
        assert_eq!(
            GuardianInputEffectQuery::new(0, 19, [0x3c; 32]),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );
        assert_eq!(
            GuardianInputEffectQuery::new(7, 0, [0x3c; 32]),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );
        let mut query_with_trailing_bytes = query.encode().to_vec();
        query_with_trailing_bytes.push(0);
        assert_eq!(
            GuardianInputEffectQuery::decode(&query_with_trailing_bytes),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );

        let oversized = GuardianSpawnPayload::new(
            CommandBuilder::new("x".repeat(GUARDIAN_MAX_PAYLOAD_BYTES)),
            pty_size(24, 80),
        )
        .unwrap();
        assert_eq!(
            oversized.encode(),
            Err(GuardianProtocolError::PayloadTooLarge)
        );
    }

    #[test]
    fn authenticated_request_payload_can_be_wiped_or_transferred_without_plain_drop() {
        let sensitive = b"input-secret-owned-by-the-request-envelope".to_vec();
        let header = GuardianRequestHeader::new(
            GuardianOperation::Input,
            id(1),
            id(2),
            id(3),
            Some(id(4)),
            1,
            1,
            Some(id(5)),
            &sensitive,
        );
        let mut envelope = GuardianRequestEnvelope::new(header, sensitive.clone());
        assert_eq!(envelope.payload(), sensitive.as_slice());

        let mut authenticated = authenticate(&envelope);
        assert_eq!(
            authenticated.authenticated_payload_bytes(),
            u32::try_from(sensitive.len()).unwrap()
        );
        authenticated.zeroize_payload();
        assert!(authenticated.payload().is_empty());
        assert_eq!(
            GuardianResponseEnvelope::reply(
                &authenticated,
                &GuardianReply::InputReceipt {
                    pane_id: id(4),
                    generation: 1,
                    sequence: 1,
                    effect_id: id(5),
                    state: InputEffectState::KnownNotApplied,
                },
            )
            .unwrap()
            .header()
            .status,
            GuardianResponseStatus::Success
        );

        let transferred = authenticate(&envelope).into_zeroizing_payload();
        assert_eq!(transferred.as_slice(), sensitive.as_slice());

        envelope.zeroize_payload();
        assert!(envelope.payload().is_empty());
        let expected_digest: [u8; 32] = Sha256::digest(&sensitive).into();
        assert_eq!(envelope.header.payload_sha256, expected_digest);
    }

    #[test]
    fn checkpoint_intent_is_versioned_fixed_width_canonical_and_content_free() {
        let checkpoint_bytes = [0x41; 32];
        let output_boundary_bytes = [0x52; 32];
        let intent = GuardianCheckpointIntent::new(
            GuardianCheckpointIdentityDigest::from_bytes(checkpoint_bytes).unwrap(),
            GuardianOutputBoundaryIdentityDigest::from_bytes(output_boundary_bytes).unwrap(),
        );
        let encoded = intent.encode();
        assert_eq!(encoded.len(), GUARDIAN_CHECKPOINT_INTENT_BYTES);
        assert_eq!(&encoded[..4], CHECKPOINT_INTENT_PAYLOAD_MAGIC.as_slice());
        assert_eq!(read_u16(&encoded, 4).unwrap(), GUARDIAN_CHECKPOINT_INTENT_VERSION);
        assert_eq!(&encoded[6..8], &[0, 0]);
        assert_eq!(GuardianCheckpointIntent::decode(&encoded).unwrap(), intent);

        let checkpoint_debug = format!("{checkpoint_bytes:?}");
        let output_boundary_debug = format!("{output_boundary_bytes:?}");
        for diagnostic in [
            format!("{:?}", intent.checkpoint_identity()),
            format!("{:?}", intent.output_boundary_identity()),
            format!("{intent:?}"),
        ] {
            assert!(!diagnostic.contains(&checkpoint_debug));
            assert!(!diagnostic.contains(&output_boundary_debug));
        }
        assert_eq!(intent.checkpoint_identity().into_bytes(), checkpoint_bytes);
        assert_eq!(
            intent.output_boundary_identity().into_bytes(),
            output_boundary_bytes
        );

        let mut noncanonical_reserved = encoded;
        noncanonical_reserved[7] = 1;
        assert_eq!(
            GuardianCheckpointIntent::decode(&noncanonical_reserved),
            Err(GuardianProtocolError::InvalidCheckpointIntent)
        );
        let mut unsupported_intent_version = encoded;
        unsupported_intent_version[4..6].copy_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(
            GuardianCheckpointIntent::decode(&unsupported_intent_version),
            Err(GuardianProtocolError::InvalidCheckpointIntent)
        );
        let mut absent_checkpoint_identity = encoded;
        absent_checkpoint_identity[8..40].fill(0);
        assert_eq!(
            GuardianCheckpointIntent::decode(&absent_checkpoint_identity),
            Err(GuardianProtocolError::InvalidCheckpointIntent)
        );
        let mut absent_output_boundary_identity = encoded;
        absent_output_boundary_identity[40..72].fill(0);
        assert_eq!(
            GuardianCheckpointIntent::decode(&absent_output_boundary_identity),
            Err(GuardianProtocolError::InvalidCheckpointIntent)
        );
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert_eq!(
            GuardianCheckpointIntent::decode(&trailing),
            Err(GuardianProtocolError::InvalidCheckpointIntent)
        );

        let checkpoint = checkpoint_request(id(1), id(2), id(3), 1, 1, 60, 61, 0x41, 0x52);
        let identity = checkpoint_effect_identity(&checkpoint);
        let pending = GuardianCheckpointPending { identity };
        let receipt = GuardianCheckpointReceipt::from_identity(
            identity,
            GuardianCheckpointDisposition::OutcomeIndeterminate,
        );
        for diagnostic in [
            format!("{identity:?}"),
            format!("{pending:?}"),
            format!("{receipt:?}"),
        ] {
            assert!(!diagnostic.contains(&checkpoint_debug));
            assert!(!diagnostic.contains(&output_boundary_debug));
        }

        let mut protocol_v1 = encode_guardian_request(&secret(), &checkpoint).unwrap();
        protocol_v1[8..10].copy_from_slice(&1_u16.to_be_bytes());
        let mac_start = protocol_v1.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&protocol_v1[..mac_start]).unwrap();
        protocol_v1[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_request(&secret(), &protocol_v1),
            Err(GuardianProtocolError::UnsupportedVersion(1))
        );
    }

    #[test]
    fn payload_free_operations_reject_authenticated_trailing_bytes() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        for (index, operation) in [
            GuardianOperation::Claim,
            GuardianOperation::Attach,
            GuardianOperation::Close,
            GuardianOperation::Replay,
            GuardianOperation::RetireLease,
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = Uuid::from_u128(0x100 + index as u128);
            let effect_id = operation
                .creates_effect()
                .then(|| Uuid::from_u128(0x200 + index as u128));
            let envelope = request(
                operation,
                guardian,
                mux,
                request_id,
                Some(pane),
                u64::from(operation != GuardianOperation::Claim),
                u64::from(operation.uses_mutation_sequence()),
                effect_id,
                b"hidden-trailing-byte",
            );
            assert_eq!(
                encode_guardian_request(&secret(), &envelope),
                Err(GuardianProtocolError::InvalidOperationPayload),
                "{operation:?} must reject an authenticated undeclared payload"
            );
        }
    }

    #[test]
    fn hello_authenticates_bootstrap_before_guardian_incarnation_is_known() {
        let guardian = id(1);
        let mux = id(2);
        let hello = request(
            GuardianOperation::Hello,
            Uuid::nil(),
            mux,
            id(30),
            None,
            0,
            0,
            None,
            b"",
        );
        let authenticated = authenticate(&hello);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        let reply = state.apply_observation(&authenticated).unwrap();
        assert_eq!(
            reply,
            GuardianReply::Hello {
                guardian_incarnation: guardian,
            }
        );
        let response = GuardianResponseEnvelope::success(&authenticated, &reply).unwrap();
        let correlated = decode_guardian_response(
            &secret(),
            &encode_guardian_response(&secret(), &response).unwrap(),
        )
        .unwrap()
        .correlate(&authenticated.header)
        .unwrap();
        assert_eq!(correlated.success_reply(&authenticated).unwrap(), reply);

        let noncanonical_known_incarnation = request(
            GuardianOperation::Hello,
            guardian,
            mux,
            id(31),
            None,
            0,
            0,
            None,
            b"",
        );
        assert_eq!(
            encode_guardian_request(&secret(), &noncanonical_known_incarnation),
            Err(GuardianProtocolError::InvalidOperationScope {
                operation: GuardianOperation::Hello,
            })
        );
        let hidden_payload = request(
            GuardianOperation::Hello,
            Uuid::nil(),
            mux,
            id(32),
            None,
            0,
            0,
            None,
            b"hidden",
        );
        assert_eq!(
            encode_guardian_request(&secret(), &hidden_payload),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );
        let census_page = GuardianCensusPageRequest::new(
            Uuid::nil(),
            0,
            1,
            GUARDIAN_MIN_CENSUS_PAGE_BYTES,
        )
        .unwrap();
        let nil_guardian_census = request(
            GuardianOperation::Census,
            Uuid::nil(),
            mux,
            id(33),
            None,
            0,
            0,
            None,
            &census_page.encode(),
        );
        assert_eq!(
            encode_guardian_request(&secret(), &nil_guardian_census),
            Err(GuardianProtocolError::ZeroIdentity("guardian incarnation")),
            "nil guardian scope must remain exclusive to the authenticated Hello bootstrap"
        );
    }

    #[test]
    fn guarded_stop_is_authenticated_process_scoped_and_exactly_correlated() {
        let guardian = id(1);
        let mux = id(2);
        let request_id = id(3);
        let effect_id = id(4);
        let stop = request(
            GuardianOperation::GuardedStop,
            guardian,
            mux,
            request_id,
            None,
            0,
            0,
            Some(effect_id),
            b"",
        );
        let authenticated = authenticate(&stop);
        let response = GuardianResponseEnvelope::success(
            &authenticated,
            &GuardianReply::GuardedStopAccepted,
        )
        .unwrap();
        let response_frame = encode_guardian_response(&secret(), &response).unwrap();
        let decoded = decode_guardian_response(&secret(), &response_frame).unwrap();
        let correlated = decoded.clone().correlate(authenticated.header()).unwrap();
        assert_eq!(
            correlated.success_reply(&authenticated).unwrap(),
            GuardianReply::GuardedStopAccepted
        );

        let different_effect = request(
            GuardianOperation::GuardedStop,
            guardian,
            mux,
            request_id,
            None,
            0,
            0,
            Some(id(5)),
            b"",
        );
        assert_eq!(
            decoded.correlate(authenticate(&different_effect).header()),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );

        let mut tampered = response_frame;
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(
            decode_guardian_response(&secret(), &tampered),
            Err(GuardianProtocolError::AuthenticationFailed)
        );

        for invalid in [
            request(
                GuardianOperation::GuardedStop,
                guardian,
                mux,
                id(6),
                None,
                0,
                0,
                None,
                b"",
            ),
            request(
                GuardianOperation::GuardedStop,
                guardian,
                mux,
                id(7),
                Some(id(8)),
                0,
                0,
                Some(id(9)),
                b"",
            ),
            request(
                GuardianOperation::GuardedStop,
                guardian,
                mux,
                id(10),
                None,
                1,
                0,
                Some(id(11)),
                b"",
            ),
        ] {
            assert_eq!(
                encode_guardian_request(&secret(), &invalid),
                Err(GuardianProtocolError::InvalidOperationScope {
                    operation: GuardianOperation::GuardedStop,
                })
            );
        }
        let trailing = request(
            GuardianOperation::GuardedStop,
            guardian,
            mux,
            id(12),
            None,
            0,
            0,
            Some(id(13)),
            b"not-empty",
        );
        assert_eq!(
            encode_guardian_request(&secret(), &trailing),
            Err(GuardianProtocolError::InvalidOperationPayload)
        );
        let owned = GuardianRejectionCode::OwnedPanesPresent;
        assert_eq!(owned.status(), GuardianResponseStatus::Rejected);
        assert_eq!(
            GuardianRejectionCode::decode(owned.status(), &owned.encode()),
            Ok(owned)
        );
    }

    #[test]
    fn failed_runtime_effect_does_not_publish_spawn_or_consume_replay_identity() {
        let sensitive_effect_error = GuardianEffectTransactionError::Effect(
            std::io::Error::other("raw-input-or-dictionary-testable-digest"),
        );
        assert!(
            !format!("{sensitive_effect_error:?}")
                .contains("raw-input-or-dictionary-testable-digest")
        );
        assert!(
            !sensitive_effect_error
                .to_string()
                .contains("raw-input-or-dictionary-testable-digest")
        );
        assert!(std::error::Error::source(&sensitive_effect_error).is_none());

        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let request = authenticate(&spawn_request(guardian, mux, pane));
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        let invocations = std::cell::Cell::new(0usize);

        let failed = state.apply_effect_transactionally(&request, |_| {
            invocations.set(invocations.get() + 1);
            GuardianEffectOutcome::DefinitelyNotApplied("injected spawn failure")
        });
        assert!(!format!("{failed:?}").contains("injected spawn failure"));
        assert!(
            !failed
                .as_ref()
                .expect_err("fixture effect fails")
                .to_string()
                .contains("injected spawn failure")
        );
        assert!(matches!(
            failed,
            Err(GuardianEffectTransactionError::Effect("injected spawn failure"))
        ));
        assert_eq!(invocations.get(), 1);
        assert_eq!(state.pane_state(pane), None);

        let first = state
            .apply_effect_transactionally(&request, |_| {
                invocations.set(invocations.get() + 1);
                GuardianEffectOutcome::<&str>::Applied
            })
            .unwrap();
        assert_eq!(invocations.get(), 2);
        assert_eq!(
            state.pane_state(pane),
            Some(&GuardianPaneState::LiveUnclaimed { generation: 0 })
        );

        let replay = state
            .apply_effect_transactionally(&request, |_| {
                invocations.set(invocations.get() + 1);
                GuardianEffectOutcome::<&str>::Applied
            })
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(invocations.get(), 2, "exact replay must not respawn");

        let mut alias_envelope = request.envelope().clone();
        alias_envelope.header.request_id = id(10);
        let alias = authenticate(&alias_envelope);
        let alias_reply = state
            .apply_effect_transactionally(&alias, |_| {
                invocations.set(invocations.get() + 1);
                GuardianEffectOutcome::<&str>::Applied
            })
            .unwrap();
        assert_eq!(alias_reply, first);
        assert_eq!(
            invocations.get(),
            2,
            "same-effect request alias must not respawn"
        );
    }

    #[test]
    fn indeterminate_effect_and_callback_panic_quarantine_exact_identity_without_retry() {
        let guardian = id(1);
        let mux = id(2);
        let mut state = GuardianProtocolState::new(guardian).unwrap();

        for (pane, request_byte, effect_byte, panic_in_callback) in
            [(id(40), 41_u8, 42_u8, false), (id(43), 44_u8, 45_u8, true)]
        {
            let request = authenticate(&request(
                GuardianOperation::Spawn,
                guardian,
                mux,
                id(request_byte),
                Some(pane),
                0,
                0,
                Some(id(effect_byte)),
                &spawn_payload("indeterminate-effect-fixture"),
            ));
            let invocations = std::cell::Cell::new(0_usize);
            let first = state.apply_effect_transactionally(&request, |_| {
                invocations.set(invocations.get() + 1);
                if panic_in_callback {
                    panic!("injected guardian effect callback panic");
                }
                GuardianEffectOutcome::<&str>::OutcomeIndeterminate
            });
            let intended_reply = match first {
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(reply)) => reply,
                other => panic!("expected indeterminate transaction, got {other:?}"),
            };
            assert_eq!(invocations.get(), 1);
            assert_eq!(
                state.pane_state(pane),
                Some(&GuardianPaneState::Quarantined {
                    generation: 0,
                    reason: GuardianQuarantineReason::EffectOutcomeIndeterminate,
                    exit_status: None,
                })
            );
            let indeterminate_reply =
                GuardianReply::effect_outcome_indeterminate(&request, &intended_reply).unwrap();
            assert_eq!(
                indeterminate_reply,
                GuardianReply::EffectOutcomeIndeterminate {
                    pane_id: pane,
                    generation: 0,
                    sequence: 0,
                    effect_id: id(effect_byte),
                }
            );
            assert_eq!(
                state.indeterminate_effect_reply(&request).unwrap(),
                Some(indeterminate_reply.clone())
            );
            let response = GuardianResponseEnvelope::reply(&request, &indeterminate_reply).unwrap();
            assert_eq!(response.header().status, GuardianResponseStatus::Indeterminate);
            let frame = encode_guardian_response(&secret(), &response).unwrap();
            let correlated = decode_guardian_response(&secret(), &frame)
                .unwrap()
                .correlate(request.header())
                .unwrap();
            assert_eq!(
                correlated.typed_reply(&request).unwrap(),
                indeterminate_reply
            );
            assert_eq!(
                correlated.success_reply(&request),
                Err(GuardianProtocolError::NonSuccessResponse)
            );

            let replay = state.apply_effect_transactionally(&request, |_| {
                invocations.set(invocations.get() + 1);
                GuardianEffectOutcome::<&str>::Applied
            });
            assert!(matches!(
                replay,
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(_))
            ));
            assert_eq!(
                invocations.get(),
                1,
                "an indeterminate exact identity must never invoke the effect again"
            );

            let mut alias_envelope = request.envelope().clone();
            alias_envelope.header.request_id = id(effect_byte + 20);
            let alias = authenticate(&alias_envelope);
            assert_eq!(
                state.indeterminate_effect_reply(&alias).unwrap(),
                Some(
                    GuardianReply::effect_outcome_indeterminate(&alias, &intended_reply).unwrap()
                )
            );
            assert!(matches!(
                state.apply_effect_transactionally(&alias, |_| {
                    invocations.set(invocations.get() + 1);
                    GuardianEffectOutcome::<&str>::Applied
                }),
                Err(GuardianEffectTransactionError::OutcomeIndeterminate(_))
            ));
            assert_eq!(invocations.get(), 1);
            assert!(state.requests.contains_key(&alias.header().request_id));

            let mut conflicting_envelope = request.envelope().clone();
            conflicting_envelope.header.effect_id = Some(id(effect_byte + 40));
            let conflicting = authenticate(&conflicting_envelope);
            assert_eq!(
                state.indeterminate_effect_reply(&conflicting),
                Err(GuardianProtocolError::RequestIdentityConflict)
            );
        }
    }

    #[test]
    fn failed_runtime_effect_does_not_evict_historical_receipts() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new_with_receipt_capacity(guardian, 2).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        let claim = claim_request(guardian, mux, pane, 0, 6, 7);
        let claimed = apply_request(&mut state, &claim).unwrap();
        assert_eq!(state.requests.len(), 2);
        assert_eq!(state.effects.len(), 2);

        let resize = authenticate(&request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(8),
            Some(pane),
            1,
            1,
            Some(id(9)),
            &resize_payload(25, 81),
        ));
        assert!(matches!(
            state.apply_effect_transactionally(&resize, |_| {
                GuardianEffectOutcome::DefinitelyNotApplied("injected resize failure")
            }),
            Err(GuardianEffectTransactionError::Effect(
                "injected resize failure"
            ))
        ));
        assert!(state.requests.contains_key(&id(6)));
        assert!(state.effects.contains_key(&id(7)));
        assert!(!state.requests.contains_key(&id(8)));
        assert!(!state.effects.contains_key(&id(9)));
        assert_eq!(state.requests.len(), 2);
        assert_eq!(state.effects.len(), 2);

        let replay_callback_invoked = std::cell::Cell::new(false);
        let replay = state
            .apply_effect_transactionally(&authenticate(&claim), |_| {
                replay_callback_invoked.set(true);
                GuardianEffectOutcome::<&str>::Applied
            })
            .unwrap();
        assert_eq!(replay, claimed);
        assert!(!replay_callback_invoked.get());
    }

    #[test]
    fn failed_runtime_mutation_does_not_advance_lease_sequence() {
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
        let input = authenticate(&request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(8),
            Some(pane),
            1,
            1,
            Some(id(9)),
            b"effect-once",
        ));

        let generic_callback_invoked = std::cell::Cell::new(false);
        assert!(matches!(
            state.apply_effect_transactionally(&input, |_| {
                generic_callback_invoked.set(true);
                GuardianEffectOutcome::<&str>::Applied
            }),
            Err(GuardianEffectTransactionError::Protocol(
                GuardianProtocolError::InvalidOperationScope {
                    operation: GuardianOperation::Input
                }
            ))
        ));
        assert!(!generic_callback_invoked.get());

        let failed =
            state.apply_input_effect_transactionally(&input, |_| Err("zero-byte write"));
        assert!(matches!(
            failed,
            Err(GuardianEffectTransactionError::Effect("zero-byte write"))
        ));
        assert_eq!(
            state.pane_state(pane),
            Some(&GuardianPaneState::LiveClaimed {
                generation: 1,
                mux_incarnation: mux,
                next_sequence: 1,
                pending_input_effect: None,
            })
        );

        let accepted = state
            .apply_input_effect_transactionally(&input, |_| Ok::<(), &str>(()))
            .unwrap();
        assert_eq!(
            accepted,
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: id(9),
                state: InputEffectState::AcceptedNotDurable,
            }
        );
        assert_eq!(
            state.pane_state(pane),
            Some(&GuardianPaneState::LiveClaimed {
                generation: 1,
                mux_incarnation: mux,
                next_sequence: 2,
                pending_input_effect: Some(id(9)),
            })
        );
    }

    #[test]
    fn committed_checkpoint_is_typed_and_exact_replays_never_publish_twice() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let checkpoint = checkpoint_request(guardian, mux, pane, 1, 1, 8, 9, 0x31, 0x32);
        let authenticated_checkpoint = authenticate(&checkpoint);
        let expected_identity = checkpoint_effect_identity(&checkpoint);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let generic_invocations = std::cell::Cell::new(0_usize);
        let generic = state.apply_effect_transactionally(&authenticated_checkpoint, |_| {
            generic_invocations.set(generic_invocations.get() + 1);
            GuardianEffectOutcome::<&str>::Applied
        });
        assert!(matches!(
            generic,
            Err(GuardianEffectTransactionError::Protocol(
                GuardianProtocolError::CheckpointRequiresTypedTransaction
            ))
        ));
        assert_eq!(generic_invocations.get(), 0);

        let invocations = std::cell::Cell::new(0_usize);
        let observed_identity = std::cell::Cell::new(None);
        let receipt = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |pending| {
                invocations.set(invocations.get() + 1);
                observed_identity.set(Some(pending.identity()));
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(invocations.get(), 1);
        assert_eq!(observed_identity.get(), Some(expected_identity));
        assert_eq!(expected_identity.pane_id(), pane);
        assert_eq!(expected_identity.mux_incarnation(), mux);
        assert_eq!(expected_identity.request_id(), id(8));
        assert_eq!(expected_identity.generation(), 1);
        assert_eq!(expected_identity.sequence(), 1);
        assert_eq!(expected_identity.effect_id(), id(9));
        assert_eq!(expected_identity.intent(), checkpoint_intent(0x31, 0x32));
        assert_eq!(receipt.disposition(), GuardianCheckpointDisposition::Committed);
        assert_eq!(receipt.pane_id(), pane);
        assert_eq!(receipt.generation(), 1);
        assert_eq!(receipt.sequence(), 1);
        assert_eq!(receipt.effect_id(), id(9));
        assert_eq!(receipt.intent(), checkpoint_intent(0x31, 0x32));

        let exact_replay = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(exact_replay, receipt);
        assert_eq!(invocations.get(), 1);

        let mut alias_envelope = checkpoint.clone();
        alias_envelope.header.request_id = id(10);
        let authenticated_alias = authenticate(&alias_envelope);
        let alias_receipt = state
            .apply_checkpoint_transactionally(&authenticated_alias, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(alias_receipt, receipt);
        assert_eq!(invocations.get(), 1);

        let response = GuardianResponseEnvelope::reply(
            &authenticated_alias,
            &GuardianReply::CheckpointReceipt(alias_receipt),
        )
        .unwrap();
        assert_eq!(response.header.status, GuardianResponseStatus::Success);
        let frame = encode_guardian_response(&secret(), &response).unwrap();
        let correlated = decode_guardian_response(&secret(), &frame)
            .unwrap()
            .correlate(&authenticated_alias.header)
            .unwrap();
        assert_eq!(
            correlated.success_reply(&authenticated_alias).unwrap(),
            GuardianReply::CheckpointReceipt(receipt)
        );
        let mut forged_indeterminate = frame;
        forged_indeterminate[11] = GuardianResponseStatus::Indeterminate as u8;
        let mac_start = forged_indeterminate.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&forged_indeterminate[..mac_start]).unwrap();
        forged_indeterminate[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_response(&secret(), &forged_indeterminate),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );

        let conflicting = checkpoint_request(guardian, mux, pane, 1, 1, 11, 9, 0x31, 0x33);
        let authenticated_conflicting = authenticate(&conflicting);
        assert_eq!(
            state.apply_checkpoint_transactionally(&authenticated_conflicting, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            }),
            Err(GuardianProtocolError::EffectIdentityConflict)
        );
        assert_eq!(invocations.get(), 1);
    }

    #[test]
    fn indeterminate_checkpoint_is_retained_fenced_visible_and_exactly_reconciled() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let checkpoint_effect = id(9);
        let checkpoint = checkpoint_request(guardian, mux, pane, 1, 1, 8, 9, 0x41, 0x52);
        let authenticated_checkpoint = authenticate(&checkpoint);
        let identity = checkpoint_effect_identity(&checkpoint);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let invocations = std::cell::Cell::new(0_usize);
        let receipt = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                invocations.set(invocations.get() + 1);
                Err("content-derived-publisher-error-must-not-escape")
            })
            .unwrap();
        assert_eq!(invocations.get(), 1);
        assert_eq!(
            receipt.disposition(),
            GuardianCheckpointDisposition::OutcomeIndeterminate
        );
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::LiveClaimed {
                generation: 1,
                mux_incarnation,
                next_sequence: 2,
                pending_input_effect: None,
            }) if *mux_incarnation == mux
        ));

        let exact_replay = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(exact_replay, receipt);
        let mut alias_envelope = checkpoint.clone();
        alias_envelope.header.request_id = id(10);
        let authenticated_alias = authenticate(&alias_envelope);
        let alias_receipt = state
            .apply_checkpoint_transactionally(&authenticated_alias, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(alias_receipt, receipt);
        assert_eq!(invocations.get(), 1);

        for index in 0..(GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT - 2) {
            let mut bounded_alias = checkpoint.clone();
            bounded_alias.header.request_id =
                Uuid::from_u128(0x1000 + u128::try_from(index).unwrap());
            assert_eq!(
                state
                    .apply_checkpoint_transactionally(&authenticate(&bounded_alias), |_| {
                        invocations.set(invocations.get() + 1);
                        Ok::<(), &str>(())
                    })
                    .unwrap(),
                receipt
            );
        }
        let mut over_alias_ceiling = checkpoint.clone();
        over_alias_ceiling.header.request_id = Uuid::from_u128(0x2000);
        assert_eq!(
            state.apply_checkpoint_transactionally(&authenticate(&over_alias_ceiling), |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            }),
            Err(GuardianProtocolError::RequestAliasCapacityExhausted {
                effect_id: checkpoint_effect,
                max_aliases: GUARDIAN_MAX_REQUEST_ALIASES_PER_PENDING_EFFECT,
            })
        );
        assert_eq!(invocations.get(), 1);

        let response = GuardianResponseEnvelope::reply(
            &authenticated_alias,
            &GuardianReply::CheckpointReceipt(alias_receipt),
        )
        .unwrap();
        assert_eq!(
            response.header.status,
            GuardianResponseStatus::Indeterminate
        );
        assert_eq!(
            GuardianResponseEnvelope::success(
                &authenticated_alias,
                &GuardianReply::CheckpointReceipt(alias_receipt),
            ),
            Err(GuardianProtocolError::NonSuccessResponse)
        );
        let frame = encode_guardian_response(&secret(), &response).unwrap();
        let correlated = decode_guardian_response(&secret(), &frame)
            .unwrap()
            .correlate(&authenticated_alias.header)
            .unwrap();
        assert_eq!(
            correlated.typed_reply(&authenticated_alias).unwrap(),
            GuardianReply::CheckpointReceipt(receipt)
        );
        assert_eq!(
            correlated.success_reply(&authenticated_alias),
            Err(GuardianProtocolError::NonSuccessResponse)
        );
        assert_eq!(
            correlated.rejection_code(),
            Err(GuardianProtocolError::InvalidRejectionPayload)
        );
        let mut forged_success = frame;
        forged_success[11] = GuardianResponseStatus::Success as u8;
        let mac_start = forged_success.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&forged_success[..mac_start]).unwrap();
        forged_success[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_response(&secret(), &forged_success),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );

        let blocked_effects = vec![
            request(
                GuardianOperation::Claim,
                guardian,
                mux,
                id(20),
                Some(pane),
                1,
                0,
                Some(id(21)),
                b"",
            ),
            request(
                GuardianOperation::Input,
                guardian,
                mux,
                id(22),
                Some(pane),
                1,
                2,
                Some(id(23)),
                b"blocked-input",
            ),
            request(
                GuardianOperation::Resize,
                guardian,
                mux,
                id(24),
                Some(pane),
                1,
                2,
                Some(id(25)),
                &resize_payload(30, 100),
            ),
            request(
                GuardianOperation::Signal,
                guardian,
                mux,
                id(26),
                Some(pane),
                1,
                2,
                Some(id(27)),
                &GuardianSignal::Terminate.encode(),
            ),
            request(
                GuardianOperation::RetireLease,
                guardian,
                mux,
                id(28),
                Some(pane),
                1,
                2,
                Some(id(29)),
                b"",
            ),
            request(
                GuardianOperation::Close,
                guardian,
                mux,
                id(30),
                Some(pane),
                1,
                2,
                Some(id(31)),
                b"",
            ),
        ];
        let blocked_callbacks = std::cell::Cell::new(0_usize);
        for blocked in blocked_effects {
            let authenticated_blocked = authenticate(&blocked);
            let blocked_result = if blocked.header.operation == GuardianOperation::Input {
                state.apply_input_effect_transactionally(&authenticated_blocked, |_| {
                    blocked_callbacks.set(blocked_callbacks.get() + 1);
                    Ok::<(), &str>(())
                })
            } else {
                state.apply_effect_transactionally(&authenticated_blocked, |_| {
                    blocked_callbacks.set(blocked_callbacks.get() + 1);
                    GuardianEffectOutcome::<&str>::Applied
                })
            };
            assert!(matches!(
                blocked_result,
                Err(GuardianEffectTransactionError::Protocol(
                    GuardianProtocolError::CheckpointOutcomeIndeterminate
                ))
            ));
        }
        let second_checkpoint = checkpoint_request(
            guardian, mux, pane, 1, 2, 32, 33, 0x61, 0x62,
        );
        assert_eq!(
            state.apply_checkpoint_transactionally(&authenticate(&second_checkpoint), |_| {
                blocked_callbacks.set(blocked_callbacks.get() + 1);
                Ok::<(), &str>(())
            }),
            Err(GuardianProtocolError::CheckpointOutcomeIndeterminate)
        );
        assert_eq!(blocked_callbacks.get(), 0);
        assert_eq!(
            state.retire_disconnected_mux_leases(mux).unwrap(),
            GuardianMuxLeaseRetirement {
                retired_panes: 0,
                pending_input_panes: 0,
                indeterminate_checkpoint_panes: 1,
            }
        );

        let census_page = GuardianCensusPageRequest::new(
            Uuid::nil(),
            0,
            1,
            GUARDIAN_MIN_CENSUS_PAGE_BYTES,
        )
        .unwrap();
        let census_request = request(
            GuardianOperation::Census,
            guardian,
            mux,
            id(34),
            None,
            0,
            0,
            None,
            &census_page.encode(),
        );
        let census = apply_request(&mut state, &census_request).unwrap();
        assert!(matches!(
            census,
            GuardianReply::CensusPage { entries, .. }
                if entries.len() == 1
                    && entries[0].pane_id == pane
                    && entries[0].next_sequence == Some(2)
                    && entries[0].indeterminate_checkpoint_effect == Some(checkpoint_effect)
        ));

        let removed_alias = state.requests.remove(&id(10)).unwrap();
        let corrupted_before_reconciliation = state.clone();
        assert_eq!(
            state.mark_checkpoint_committed(identity),
            Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-reconciliation-request-alias"
            ))
        );
        assert_eq!(state, corrupted_before_reconciliation);
        state.requests.insert(id(10), removed_alias);

        let alias_identity = checkpoint_effect_identity(&alias_envelope);
        let before_alias_identity_mismatch = state.clone();
        assert_eq!(
            state.mark_checkpoint_committed(alias_identity),
            Err(GuardianProtocolError::CheckpointIdentityMismatch)
        );
        assert_eq!(state, before_alias_identity_mismatch);
        let mut wrong_output_boundary = identity;
        wrong_output_boundary.intent = checkpoint_intent(0x41, 0x53);
        let before_output_boundary_mismatch = state.clone();
        assert_eq!(
            state.mark_checkpoint_committed(wrong_output_boundary),
            Err(GuardianProtocolError::CheckpointIdentityMismatch)
        );
        assert_eq!(state, before_output_boundary_mismatch);

        let committed = state.mark_checkpoint_committed(identity).unwrap();
        assert_eq!(
            committed.disposition(),
            GuardianCheckpointDisposition::Committed
        );
        assert_eq!(state.mark_checkpoint_committed(identity).unwrap(), committed);
        let committed_alias = state
            .apply_checkpoint_transactionally(&authenticated_alias, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(committed_alias, committed);
        assert_eq!(invocations.get(), 1);
        assert_eq!(
            state.mark_checkpoint_definitely_not_published(identity),
            Err(GuardianProtocolError::CheckpointIdentityMismatch)
        );

        let successor = request(
            GuardianOperation::Resize,
            guardian,
            mux,
            id(35),
            Some(pane),
            1,
            2,
            Some(id(36)),
            &resize_payload(31, 101),
        );
        assert!(apply_request(&mut state, &successor).is_ok());

        let checkpoint_debug = format!("{:?}", [0x41_u8; 32]);
        let output_boundary_debug = format!("{:?}", [0x52_u8; 32]);
        let state_debug = format!("{state:?}");
        assert!(!state_debug.contains("content-derived-publisher-error-must-not-escape"));
        assert!(!state_debug.contains(&checkpoint_debug));
        assert!(!state_debug.contains(&output_boundary_debug));
    }

    #[test]
    fn checkpoint_panic_and_preflight_failures_are_non_reinvoking_and_rollback_is_exact() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let checkpoint = checkpoint_request(guardian, mux, pane, 1, 1, 8, 9, 0x71, 0x72);
        let authenticated_checkpoint = authenticate(&checkpoint);
        let identity = checkpoint_effect_identity(&checkpoint);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

        let sequence_gap = checkpoint_request(guardian, mux, pane, 1, 2, 10, 11, 0x73, 0x74);
        let preflight_invocations = std::cell::Cell::new(0_usize);
        let before_sequence_gap = state.clone();
        assert_eq!(
            state.apply_checkpoint_transactionally(&authenticate(&sequence_gap), |_| {
                preflight_invocations.set(preflight_invocations.get() + 1);
                Ok::<(), &str>(())
            }),
            Err(GuardianProtocolError::SequenceGap {
                expected: 1,
                observed: 2,
            })
        );
        assert_eq!(preflight_invocations.get(), 0);
        assert_eq!(state, before_sequence_gap);

        let saved_request_order = state.transient_request_order.clone();
        let saved_effect_order = state.transient_effect_order.clone();
        let saved_capacity = state.receipt_capacity;
        state.receipt_capacity = state.requests.len();
        state.transient_request_order.clear();
        state.transient_effect_order.clear();
        let before_capacity_failure = state.clone();
        assert_eq!(
            state.apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                preflight_invocations.set(preflight_invocations.get() + 1);
                Ok::<(), &str>(())
            }),
            Err(GuardianProtocolError::CapacityExhausted)
        );
        assert_eq!(preflight_invocations.get(), 0);
        assert_eq!(state, before_capacity_failure);
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::LiveClaimed {
                next_sequence: 1,
                ..
            })
        ));
        state.receipt_capacity = saved_capacity;
        state.transient_request_order = saved_request_order;
        state.transient_effect_order = saved_effect_order;

        let invocations = std::cell::Cell::new(0_usize);
        let indeterminate = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| -> Result<(), &str> {
                invocations.set(invocations.get() + 1);
                panic!("checkpoint publisher panic");
            })
            .unwrap();
        assert_eq!(invocations.get(), 1);
        assert_eq!(
            indeterminate.disposition(),
            GuardianCheckpointDisposition::OutcomeIndeterminate
        );
        assert_eq!(
            state
                .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                    invocations.set(invocations.get() + 1);
                    Ok::<(), &str>(())
                })
                .unwrap(),
            indeterminate
        );
        assert_eq!(invocations.get(), 1);

        let reverse_index = state
            .effect_request_ids
            .remove(&identity.effect_id())
            .unwrap();
        let missing_reverse_index = state.clone();
        assert_eq!(
            state.mark_checkpoint_definitely_not_published(identity),
            Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-reverse-index"
            ))
        );
        assert_eq!(state, missing_reverse_index);
        state
            .effect_request_ids
            .insert(identity.effect_id(), reverse_index);

        let removed_request = state.requests.remove(&identity.request_id()).unwrap();
        let missing_request_alias = state.clone();
        assert_eq!(
            state.mark_checkpoint_definitely_not_published(identity),
            Err(GuardianProtocolError::StateInvariantViolation(
                "checkpoint-definite-not-published-request-alias"
            ))
        );
        assert_eq!(state, missing_request_alias);
        state
            .requests
            .insert(identity.request_id(), removed_request);

        let mut wrong_request_nonce = identity;
        wrong_request_nonce.request_id = id(12);
        let before_wrong_identity = state.clone();
        assert_eq!(
            state.mark_checkpoint_definitely_not_published(wrong_request_nonce),
            Err(GuardianProtocolError::CheckpointIdentityMismatch)
        );
        assert_eq!(state, before_wrong_identity);
        let GuardianPaneState::LiveClaimed { next_sequence, .. } =
            state.panes.get_mut(&pane).unwrap()
        else {
            panic!("checkpoint pane must remain claimed while reconciliation is pending");
        };
        *next_sequence = 3;
        assert_eq!(
            state.mark_checkpoint_definitely_not_published(identity),
            Err(GuardianProtocolError::CheckpointIdentityMismatch)
        );
        assert!(state.effects.contains_key(&identity.effect_id()));
        let GuardianPaneState::LiveClaimed { next_sequence, .. } =
            state.panes.get_mut(&pane).unwrap()
        else {
            panic!("checkpoint pane must remain claimed after rejected reconciliation");
        };
        *next_sequence = 2;
        state
            .mark_checkpoint_definitely_not_published(identity)
            .unwrap();
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::LiveClaimed {
                next_sequence: 1,
                ..
            })
        ));
        assert!(!state.effects.contains_key(&identity.effect_id()));
        assert!(!state.requests.contains_key(&identity.request_id()));
        assert!(!state.indeterminate_checkpoints_by_pane.contains_key(&pane));

        let committed = state
            .apply_checkpoint_transactionally(&authenticated_checkpoint, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
            })
            .unwrap();
        assert_eq!(invocations.get(), 2);
        assert_eq!(
            committed.disposition(),
            GuardianCheckpointDisposition::Committed
        );
    }

    #[test]
    fn authenticated_frame_round_trip_and_tamper_rejection() {
        assert!(matches!(
            GuardianSecret::from_bytes([0; GUARDIAN_AUTH_TOKEN_BYTES]),
            Err(GuardianProtocolError::WeakSecret)
        ));
        assert_eq!(format!("{:?}", secret()), "GuardianSecret([REDACTED])");
        let original = spawn_request(id(1), id(2), id(3));
        let payload_digest_debug = format!("{:?}", original.header.payload_sha256);
        assert!(!format!("{original:?}").contains("bounded-command"));
        assert!(
            !format!("{original:?}").contains(&payload_digest_debug)
                && !format!("{:?}", original.header).contains(&payload_digest_debug),
            "request diagnostics must not expose a dictionary-testable payload digest"
        );
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
        let reply = GuardianReply::Spawned {
            pane_id: id(3),
            generation: 0,
        };
        let authenticated_request = authenticate(&original_request);
        let response = GuardianResponseEnvelope::success(&authenticated_request, &reply).unwrap();
        let request_digest_debug = format!("{:?}", response.header.request_payload_sha256);
        let response_digest_debug = format!("{:?}", response.header.payload_sha256);
        let response_header_debug = format!("{:?}", response.header);
        assert!(
            !response_header_debug.contains(&request_digest_debug)
                && !response_header_debug.contains(&response_digest_debug),
            "response-header diagnostics must not expose content-derived digests"
        );
        assert_eq!(
            GuardianResponseEnvelope::success(
                &authenticated_request,
                &GuardianReply::Spawned {
                    pane_id: id(8),
                    generation: 0,
                },
            ),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );
        assert!(!format!("{response:?}").contains(&id(3).to_string()));
        let frame = encode_guardian_response(&secret(), &response).unwrap();
        let authenticated = decode_guardian_response(&secret(), &frame).unwrap();
        let correlated = authenticated
            .clone()
            .correlate(&original_request.header)
            .unwrap();
        assert_eq!(correlated.envelope(), &response);
        assert_eq!(
            correlated.success_reply(&authenticated_request).unwrap(),
            reply
        );

        let mismatched_payload = GuardianReply::Spawned {
            pane_id: id(8),
            generation: 0,
        }
        .encode_for_operation(GuardianOperation::Spawn)
        .unwrap();
        let mismatched_response = GuardianResponseEnvelope {
            header: GuardianResponseHeader::new(
                &original_request.header,
                GuardianResponseStatus::Success,
                &mismatched_payload,
            ),
            payload: mismatched_payload,
        };
        assert_eq!(
            encode_guardian_response(&secret(), &mismatched_response),
            Err(GuardianProtocolError::ResponseRequestMismatch)
        );

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
        assert_eq!(
            encode_guardian_response(&secret(), &wrong_lease),
            Err(GuardianProtocolError::InvalidOperationScope {
                operation: GuardianOperation::Spawn,
            })
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
    fn authenticated_rejections_are_typed_content_free_and_status_consistent() {
        let original_request = spawn_request(id(1), id(2), id(3));
        let authenticated_request = authenticate(&original_request);
        let all_codes = [
            GuardianRejectionCode::InvalidRequest,
            GuardianRejectionCode::GuardianIncarnationMismatch,
            GuardianRejectionCode::PaneNotFound,
            GuardianRejectionCode::PaneAlreadyExists,
            GuardianRejectionCode::RequestIdentityConflict,
            GuardianRejectionCode::EffectIdentityConflict,
            GuardianRejectionCode::PaneTerminal,
            GuardianRejectionCode::ClaimGenerationMismatch,
            GuardianRejectionCode::StaleLease,
            GuardianRejectionCode::RepeatedSequence,
            GuardianRejectionCode::SequenceGap,
            GuardianRejectionCode::GenerationExhausted,
            GuardianRejectionCode::SequenceExhausted,
            GuardianRejectionCode::CapacityExhausted,
            GuardianRejectionCode::RequestAliasCapacityExhausted,
            GuardianRejectionCode::InputDurabilityPending,
            GuardianRejectionCode::InputDurabilityIdentityMismatch,
            GuardianRejectionCode::CensusSnapshotNotFound,
            GuardianRejectionCode::CensusSnapshotIdentityConflict,
            GuardianRejectionCode::InvalidCensusCursor,
            GuardianRejectionCode::InternalInvariant,
            GuardianRejectionCode::CheckpointOutcomeIndeterminate,
            GuardianRejectionCode::CheckpointIdentityMismatch,
            GuardianRejectionCode::OwnedPanesPresent,
            GuardianRejectionCode::InputKnownNotApplied,
        ];
        for code in all_codes {
            let response = GuardianResponseEnvelope::rejection(&authenticated_request, code);
            assert_eq!(response.header.status, code.status());
            assert_eq!(response.payload.len(), REJECTION_PAYLOAD_BYTES);
            let decoded = decode_guardian_response(
                &secret(),
                &encode_guardian_response(&secret(), &response).unwrap(),
            )
            .unwrap()
            .correlate(&original_request.header)
            .unwrap();
            assert_eq!(decoded.rejection_code().unwrap(), code);
        }

        let rejection = GuardianResponseEnvelope::rejection(
            &authenticated_request,
            GuardianRejectionCode::CapacityExhausted,
        );
        assert_eq!(rejection.header.status, GuardianResponseStatus::Rejected);
        assert_eq!(rejection.payload.len(), REJECTION_PAYLOAD_BYTES);
        let correlated = decode_guardian_response(
            &secret(),
            &encode_guardian_response(&secret(), &rejection).unwrap(),
        )
        .unwrap()
        .correlate(&original_request.header)
        .unwrap();
        assert_eq!(
            correlated.rejection_code().unwrap(),
            GuardianRejectionCode::CapacityExhausted
        );
        assert_eq!(
            correlated.success_reply(&authenticated_request),
            Err(GuardianProtocolError::NonSuccessResponse)
        );

        let terminal = GuardianResponseEnvelope::rejection(
            &authenticated_request,
            GuardianRejectionCode::StaleLease,
        );
        assert_eq!(terminal.header.status, GuardianResponseStatus::Terminal);
        assert_eq!(
            GuardianRejectionCode::from_protocol_error(&GuardianProtocolError::StaleLease),
            GuardianRejectionCode::StaleLease
        );
        assert_eq!(
            GuardianRejectionCode::from_protocol_error(
                &GuardianProtocolError::InputDurabilityPending
            )
            .status(),
            GuardianResponseStatus::Rejected
        );
        assert_eq!(
            GuardianRejectionCode::CheckpointOutcomeIndeterminate.status(),
            GuardianResponseStatus::Rejected,
            "a mutation blocked behind reconciliation is retryable after the exact fence clears"
        );

        let checkpoint_fence = GuardianResponseEnvelope::rejection(
            &authenticated_request,
            GuardianRejectionCode::CheckpointOutcomeIndeterminate,
        );
        let mut forged_terminal =
            encode_guardian_response(&secret(), &checkpoint_fence).unwrap();
        forged_terminal[11] = GuardianResponseStatus::Terminal as u8;
        let mac_start = forged_terminal.len() - GUARDIAN_MAC_BYTES;
        let tag = secret().mac(&forged_terminal[..mac_start]).unwrap();
        forged_terminal[mac_start..].copy_from_slice(&tag);
        assert_eq!(
            decode_guardian_response(&secret(), &forged_terminal),
            Err(GuardianProtocolError::InvalidRejectionPayload)
        );

        let mut mismatched_status = terminal.clone();
        mismatched_status.header.status = GuardianResponseStatus::Rejected;
        assert_eq!(
            encode_guardian_response(&secret(), &mismatched_status),
            Err(GuardianProtocolError::InvalidRejectionPayload)
        );
        let mut unknown_code = terminal;
        unknown_code.payload[4..].copy_from_slice(&u16::MAX.to_be_bytes());
        unknown_code.header.payload_sha256 = Sha256::digest(&unknown_code.payload).into();
        assert_eq!(
            encode_guardian_response(&secret(), &unknown_code),
            Err(GuardianProtocolError::InvalidRejectionPayload)
        );
    }

    #[test]
    fn success_reply_wire_round_trip_is_operation_typed_and_census_bounded() {
        let reply = GuardianReply::CensusPage {
            snapshot_id: id(20),
            entries: vec![
                GuardianCensusEntry {
                    pane_id: id(21),
                    status: GuardianCensusPaneStatus::LiveUnclaimed,
                    generation: 0,
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    indeterminate_checkpoint_effect: None,
                    exit_status: None,
                    quarantine_reason: None,
                },
                GuardianCensusEntry {
                    pane_id: id(22),
                    status: GuardianCensusPaneStatus::LiveClaimed,
                    generation: 3,
                    mux_incarnation: Some(id(30)),
                    next_sequence: Some(7),
                    pending_input_effect: Some(id(31)),
                    indeterminate_checkpoint_effect: None,
                    exit_status: None,
                    quarantine_reason: None,
                },
                GuardianCensusEntry {
                    pane_id: id(23),
                    status: GuardianCensusPaneStatus::ExitedUnclaimed,
                    generation: 3,
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    indeterminate_checkpoint_effect: None,
                    exit_status: Some(-9),
                    quarantine_reason: None,
                },
                GuardianCensusEntry {
                    pane_id: id(24),
                    status: GuardianCensusPaneStatus::ClosedTerminal,
                    generation: 4,
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    indeterminate_checkpoint_effect: None,
                    exit_status: Some(i32::MIN),
                    quarantine_reason: None,
                },
                GuardianCensusEntry {
                    pane_id: id(25),
                    status: GuardianCensusPaneStatus::Quarantined,
                    generation: u64::MAX,
                    mux_incarnation: None,
                    next_sequence: None,
                    pending_input_effect: None,
                    indeterminate_checkpoint_effect: None,
                    exit_status: None,
                    quarantine_reason: Some(GuardianQuarantineReason::GenerationExhausted),
                },
            ],
            next_cursor: None,
            total_panes: 5,
        };
        let encoded = reply
            .encode_for_operation(GuardianOperation::Census)
            .unwrap();
        assert_eq!(
            encoded.len(),
            usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES).unwrap()
                + 5 * usize::try_from(GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES).unwrap()
        );
        assert_eq!(
            GuardianReply::decode_for_operation(GuardianOperation::Census, &encoded).unwrap(),
            reply
        );
        let mut noncanonical_absent_exit_status = encoded.clone();
        let first_exit_status_byte = usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES).unwrap()
            + 81;
        noncanonical_absent_exit_status[first_exit_status_byte] = 1;
        assert_eq!(
            GuardianReply::decode_for_operation(
                GuardianOperation::Census,
                &noncanonical_absent_exit_status,
            ),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );

        let page = GuardianCensusPageRequest::new(
            Uuid::nil(),
            0,
            GUARDIAN_MAX_CENSUS_ENTRIES,
            GUARDIAN_MAX_CENSUS_BYTES,
        )
        .unwrap();
        let census_request = authenticate(&request(
            GuardianOperation::Census,
            id(1),
            id(2),
            id(40),
            None,
            0,
            0,
            None,
            &page.encode(),
        ));
        let response = GuardianResponseEnvelope::success(&census_request, &reply).unwrap();
        let correlated = decode_guardian_response(
            &secret(),
            &encode_guardian_response(&secret(), &response).unwrap(),
        )
        .unwrap()
        .correlate(&census_request.header)
        .unwrap();
        assert_eq!(correlated.success_reply(&census_request).unwrap(), reply);

        let entry_limited_page = GuardianCensusPageRequest::new(
            Uuid::nil(),
            0,
            4,
            GUARDIAN_MAX_CENSUS_BYTES,
        )
        .unwrap();
        let entry_limited_request = authenticate(&request(
            GuardianOperation::Census,
            id(1),
            id(2),
            id(41),
            None,
            0,
            0,
            None,
            &entry_limited_page.encode(),
        ));
        assert_eq!(
            GuardianResponseEnvelope::success(&entry_limited_request, &reply),
            Err(GuardianProtocolError::InvalidReplyPayload),
            "the server must not authenticate more entries than the exact request admitted"
        );
        let oversized_payload = reply
            .encode_for_operation(GuardianOperation::Census)
            .unwrap();
        let forged_correlated_page = CorrelatedGuardianResponse(GuardianResponseEnvelope {
            header: GuardianResponseHeader::new(
                &entry_limited_request.header,
                GuardianResponseStatus::Success,
                &oversized_payload,
            ),
            payload: oversized_payload,
        });
        assert_eq!(
            forged_correlated_page.success_reply(&entry_limited_request),
            Err(GuardianProtocolError::InvalidReplyPayload),
            "the correlated consumer must independently enforce the exact request ceiling"
        );

        let four_entry_bytes = GUARDIAN_CENSUS_PAGE_HEADER_BYTES
            + 4 * GUARDIAN_CENSUS_ENTRY_ENCODED_BYTES;
        let byte_limited_page = GuardianCensusPageRequest::new(
            Uuid::nil(),
            0,
            GUARDIAN_MAX_CENSUS_ENTRIES,
            four_entry_bytes,
        )
        .unwrap();
        let byte_limited_request = authenticate(&request(
            GuardianOperation::Census,
            id(1),
            id(2),
            id(42),
            None,
            0,
            0,
            None,
            &byte_limited_page.encode(),
        ));
        assert_eq!(
            GuardianResponseEnvelope::success(&byte_limited_request, &reply),
            Err(GuardianProtocolError::InvalidReplyPayload),
            "the server must not authenticate a page larger than the exact byte budget"
        );

        let mut false_cursor = reply.clone();
        let GuardianReply::CensusPage { next_cursor, .. } = &mut false_cursor else {
            unreachable!();
        };
        *next_cursor = Some(4);
        assert_eq!(
            GuardianResponseEnvelope::success(&census_request, &false_cursor),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );

        let mut malformed_flags = encoded;
        let first_entry_flags =
            usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES).unwrap() + 86;
        malformed_flags[first_entry_flags] = 0x80;
        assert_eq!(
            GuardianReply::decode_for_operation(GuardianOperation::Census, &malformed_flags),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );
        assert_eq!(
            GuardianReply::Spawned {
                pane_id: id(3),
                generation: 0,
            }
            .encode_for_operation(GuardianOperation::Signal),
            Err(GuardianProtocolError::ReplyOperationMismatch {
                operation: GuardianOperation::Signal,
            })
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
            Uuid::nil(),
            0,
            GUARDIAN_MAX_CENSUS_ENTRIES,
            GUARDIAN_MAX_CENSUS_BYTES,
        )
        .unwrap();
        assert_eq!(GuardianCensusPageRequest::decode(&page.encode()).unwrap(), page);
        assert_eq!(
            GuardianCensusPageRequest::new(
                Uuid::nil(),
                0,
                GUARDIAN_MAX_CENSUS_ENTRIES + 1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(Uuid::nil(), 0, 0, GUARDIAN_MIN_CENSUS_PAGE_BYTES),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(
                Uuid::nil(),
                0,
                1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES - 1,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(
                id(90),
                0,
                1,
                GUARDIAN_MIN_CENSUS_PAGE_BYTES,
            ),
            Err(GuardianProtocolError::InvalidCensusPage)
        );
        assert_eq!(
            GuardianCensusPageRequest::new(
                Uuid::nil(),
                1,
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
                snapshot_id: Uuid::from_u128(1),
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
            let payload = spawn_payload("census-pane");
            let spawn = request(
                GuardianOperation::Spawn,
                guardian,
                mux,
                id(request_byte),
                Some(id(pane_byte)),
                0,
                0,
                Some(id(effect_byte)),
                &payload,
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
        let first = apply_request(
            &mut state,
            &census(50, Uuid::nil(), 0, 2, two_entry_bytes),
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
        assert!(!first_snapshot_id.is_nil());
        let snapshot_id = first_snapshot_id;
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
                && entry.indeterminate_checkpoint_effect.is_none()
                && entry.exit_status.is_none()
                && entry.quarantine_reason.is_none()
        }));

        let conflicting_page = GuardianCensusPageRequest::new(
            snapshot_id,
            2,
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

        let concurrent_spawn_payload = spawn_payload("concurrent-census-pane");
        let concurrent_spawn = request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(43),
            Some(id(25)),
            0,
            0,
            Some(id(44)),
            &concurrent_spawn_payload,
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
                    indeterminate_checkpoint_effect: None,
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
                Uuid::nil(),
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
                total_panes: 4,
            } if observed_snapshot_id != snapshot_id
                && !observed_snapshot_id.is_nil()
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
            let page = GuardianCensusPageRequest::new(
                Uuid::nil(),
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
                } if observed == Uuid::from_u128(u128::from(offset) + 1)
                    && entries.is_empty()
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

        let retired_snapshot = Uuid::from_u128(1);
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
    fn disconnected_mux_retirement_is_incarnation_fenced_and_input_safe() {
        let guardian = id(1);
        let old_mux = id(2);
        let successor_mux = id(3);
        let first_pane = id(40);
        let pending_pane = id(41);
        let successor_pane = id(42);
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        let spawn = |pane, request_byte, effect_byte| {
            let payload = spawn_payload("lease-retirement-fixture");
            request(
                GuardianOperation::Spawn,
                guardian,
                old_mux,
                id(request_byte),
                Some(pane),
                0,
                0,
                Some(id(effect_byte)),
                &payload,
            )
        };

        apply_request(&mut state, &spawn(first_pane, 10, 11)).unwrap();
        apply_request(&mut state, &spawn(pending_pane, 12, 13)).unwrap();
        apply_request(&mut state, &spawn(successor_pane, 14, 15)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, old_mux, first_pane, 0, 16, 17),
        )
        .unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, old_mux, pending_pane, 0, 18, 19),
        )
        .unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, successor_mux, successor_pane, 0, 20, 21),
        )
        .unwrap();
        let pending_input = request(
            GuardianOperation::Input,
            guardian,
            old_mux,
            id(22),
            Some(pending_pane),
            1,
            1,
            Some(id(23)),
            b"ambiguous-before-disconnect",
        );
        apply_request(&mut state, &pending_input).unwrap();

        assert_eq!(
            state.retire_disconnected_mux_leases(old_mux).unwrap(),
            GuardianMuxLeaseRetirement {
                retired_panes: 1,
                pending_input_panes: 1,
                indeterminate_checkpoint_panes: 0,
            }
        );
        assert!(matches!(
            state.pane_state(first_pane),
            Some(GuardianPaneState::LiveUnclaimed { generation: 1 })
        ));
        assert!(matches!(
            state.pane_state(pending_pane),
            Some(GuardianPaneState::LiveClaimed {
                mux_incarnation,
                pending_input_effect: Some(effect),
                ..
            }) if *mux_incarnation == old_mux && *effect == id(23)
        ));
        assert!(matches!(
            state.pane_state(successor_pane),
            Some(GuardianPaneState::LiveClaimed { mux_incarnation, .. })
                if *mux_incarnation == successor_mux
        ));
        let stale_resize = request(
            GuardianOperation::Resize,
            guardian,
            old_mux,
            id(30),
            Some(first_pane),
            1,
            1,
            Some(id(31)),
            &resize_payload(30, 100),
        );
        assert_eq!(
            apply_request(&mut state, &stale_resize),
            Err(GuardianProtocolError::StaleLease),
            "disconnect retirement must fence the dead mux before any later effect"
        );
        assert!(!state.effects.contains_key(&id(31)));
        assert_eq!(
            apply_request(
                &mut state,
                &claim_request(guardian, successor_mux, pending_pane, 1, 24, 25),
            ),
            Err(GuardianProtocolError::InputDurabilityPending)
        );

        apply_request(
            &mut state,
            &claim_request(guardian, successor_mux, first_pane, 1, 26, 27),
        )
        .unwrap();
        state
            .mark_input_durable_full(input_effect_identity(&pending_input))
            .unwrap();
        assert_eq!(
            state.retire_disconnected_mux_leases(old_mux).unwrap(),
            GuardianMuxLeaseRetirement {
                retired_panes: 1,
                pending_input_panes: 0,
                indeterminate_checkpoint_panes: 0,
            }
        );
        apply_request(
            &mut state,
            &claim_request(guardian, successor_mux, pending_pane, 1, 28, 29),
        )
        .unwrap();
        assert_eq!(
            state.retire_disconnected_mux_leases(old_mux).unwrap(),
            GuardianMuxLeaseRetirement::default(),
            "a delayed disconnect notification must not retire successor leases"
        );
        assert_eq!(
            state.retire_disconnected_mux_leases(Uuid::nil()),
            Err(GuardianProtocolError::ZeroIdentity("mux incarnation"))
        );
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
            &resize_payload(30, 100),
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
        let input_digest_debug = format!("{:?}", input.header.payload_sha256);
        assert!(
            !format!("{:?}", input_effect_identity(&input)).contains(&input_digest_debug),
            "durability-identity diagnostics must not expose the input digest"
        );
        let query_payload = input_effect_query_payload(&input);
        let query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(22),
            Some(pane),
            1,
            0,
            Some(effect),
            &query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::NotSeen,
            },
            "only an effect at or beyond the unconsumed sequence fence may be reported as safe to resend"
        );
        let receipt = apply_request(&mut state, &input).unwrap();
        assert!(
            !format!("{state:?}").contains(&input_digest_debug),
            "state-machine diagnostics must not expose retained input digests"
        );
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
            &resize_payload(40, 120),
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
            state
                .mark_input_durable_full(input_effect_identity(&input))
                .unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableFull,
            }
        );
        assert_eq!(
            apply_request(&mut state, &retry_after_ambiguous_response).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableFull,
            }
        );
        assert_eq!(
            apply_request(&mut state, &input).unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurableFull,
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
                state: InputEffectState::DurableFull,
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
            b"",
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
            &query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &terminal_query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::DurableFull,
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

    #[cfg(unix)]
    #[test]
    fn terminal_capacity_exhaustion_never_yields_or_invokes_an_input_writer() {
        use crate::guardian_input_journal::{
            GuardianInputJournal, GuardianInputJournalError, GuardianInputJournalLimits,
            GuardianInputTransaction, GuardianInputTransactionError,
            begin_guardian_input_transaction,
        };
        use crate::guardian_output_journal::GuardianOutputCipher;
        use std::os::unix::fs::OpenOptionsExt as _;

        struct CountingWriter {
            calls: u32,
        }

        impl std::io::Write for CountingWriter {
            fn write(&mut self, payload: &[u8]) -> std::io::Result<usize> {
                self.calls = self.calls.saturating_add(1);
                Ok(payload.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

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
        let input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(70),
            Some(pane),
            1,
            1,
            Some(id(71)),
            b"x",
        );
        let authenticated_input = authenticate(&input);
        let journal_temp = tempfile::tempdir().expect("create input journal tempdir");
        let journal_path = journal_temp.path().join("input.journal");
        let journal_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&journal_path)
            .expect("create private input journal");
        let defaults = GuardianInputJournalLimits::default();
        let mut journal = GuardianInputJournal::create(
            journal_file,
            pane,
            guardian,
            GuardianOutputCipher::try_from_key_slice(&[0x92; 32])
                .expect("valid journal fixture key"),
            GuardianInputJournalLimits {
                max_records: 2,
                ..defaults
            },
        )
        .expect("initialize capacity-negative journal");
        journal
            .sync_parent_directory_and_activate(
                &std::fs::File::open(journal_temp.path()).expect("open journal directory"),
            )
            .expect("activate capacity-negative journal");

        let protocol_before = state.clone();
        let mut writer = CountingWriter { calls: 0 };
        match begin_guardian_input_transaction(&mut state, &mut journal, &authenticated_input) {
            Err(GuardianInputTransactionError::JournalBeforeWrite(
                GuardianInputJournalError::RecordLimit { maximum: 2 },
            )) => {}
            Ok(GuardianInputTransaction::WriteAuthorized { permit, .. }) => {
                let _outcome = permit.write_once(&mut writer, authenticated_input.payload());
                panic!("terminal capacity exhaustion must not yield write authority");
            }
            other => panic!("unexpected capacity-negative outcome: {other:?}"),
        }
        assert_eq!(writer.calls, 0);
        assert_eq!(journal.record_count(), 0);
        assert_eq!(state, protocol_before);
    }

    #[cfg(unix)]
    #[test]
    fn durable_partial_input_is_count_bound_and_exact_retries_never_reapply_prefix() {
        use crate::guardian_input_journal::{
            GuardianInputJournal, GuardianInputJournalLimits, GuardianInputTransaction,
            GuardianInputTransactionError, begin_guardian_input_transaction,
            commit_guardian_input_outcome,
        };
        use crate::guardian_output_journal::GuardianOutputCipher;
        use std::os::unix::fs::OpenOptionsExt as _;

        struct ThreeByteWriter {
            calls: u32,
        }

        impl std::io::Write for ThreeByteWriter {
            fn write(&mut self, _payload: &[u8]) -> std::io::Result<usize> {
                self.calls += 1;
                Ok(3)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(60);
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
            id(61),
            Some(pane),
            1,
            1,
            Some(effect),
            b"abcdef",
        );
        let authenticated_input = authenticate(&input);
        let identity = input_effect_identity(&input);
        let journal_temp = tempfile::tempdir().expect("create input journal tempdir");
        let journal_path = journal_temp.path().join("input.journal");
        let journal_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&journal_path)
            .expect("create private input journal");
        let mut journal = GuardianInputJournal::create(
            journal_file,
            pane,
            guardian,
            GuardianOutputCipher::try_from_key_slice(&[0x91; 32])
                .expect("valid journal fixture key"),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize input journal");
        journal
            .sync_parent_directory_and_activate(
                &std::fs::File::open(journal_temp.path()).expect("open journal directory"),
            )
            .expect("activate input journal");

        let (accepted, write_permit) =
            match begin_guardian_input_transaction(&mut state, &mut journal, &authenticated_input)
                .unwrap()
            {
                GuardianInputTransaction::WriteAuthorized {
                    accepted_reply,
                    permit,
                } => (accepted_reply, permit),
                GuardianInputTransaction::Reconciled(_) => {
                    panic!("new input must yield one fresh write permit")
                }
            };
        assert_eq!(write_permit.identity(), identity);
        assert!(matches!(
            accepted,
            GuardianReply::InputReceipt {
                state: InputEffectState::AcceptedNotDurable,
                ..
            }
        ));

        let before_invalid_count = state.clone();
        assert_eq!(
            state.mark_input_durable_prefix(identity, 0),
            Err(GuardianProtocolError::InvalidInputDisposition)
        );
        assert_eq!(state, before_invalid_count);
        assert_eq!(
            state.mark_input_durable_prefix(identity, identity.input_bytes()),
            Err(GuardianProtocolError::InvalidInputDisposition)
        );
        assert_eq!(state, before_invalid_count);

        let mut writer = ThreeByteWriter { calls: 0 };
        let write_outcome = write_permit.write_once(&mut writer, authenticated_input.payload());
        assert_eq!(writer.calls, 1);
        assert_eq!(write_outcome.applied_bytes(), Some(3));
        let completion = commit_guardian_input_outcome(&mut journal, write_outcome)
            .expect("persist exact partial result");
        let partial = completion.reconcile_protocol(&mut state).unwrap();
        assert_eq!(
            partial,
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::DurablePrefix { applied_bytes: 3 },
            }
        );
        assert_eq!(
            state.mark_input_durable_prefix(identity, 3).unwrap(),
            partial,
            "an exact terminal acknowledgement retry must return the original receipt"
        );
        let terminal_state = state.clone();
        assert_eq!(
            state.mark_input_durable_prefix(identity, 2),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch)
        );
        assert_eq!(state, terminal_state);

        let records_before_retries = journal.record_count();
        assert!(matches!(
            begin_guardian_input_transaction(&mut state, &mut journal, &authenticated_input),
            Ok(GuardianInputTransaction::Reconciled(reply)) if reply == partial
        ));
        let mut alias = input.clone();
        alias.header.request_id = id(62);
        assert!(matches!(
            begin_guardian_input_transaction(&mut state, &mut journal, &authenticate(&alias)),
            Ok(GuardianInputTransaction::Reconciled(reply)) if reply == partial
        ));
        assert_eq!(journal.record_count(), records_before_retries);

        let before_payload_splice = state.clone();
        let mut same_length_payload_splice = input.clone();
        same_length_payload_splice.header.request_id = id(65);
        same_length_payload_splice.payload = b"abcxef".to_vec();
        same_length_payload_splice.header.payload_sha256 =
            Sha256::digest(&same_length_payload_splice.payload).into();
        assert!(matches!(
            begin_guardian_input_transaction(
                &mut state,
                &mut journal,
                &authenticate(&same_length_payload_splice),
            ),
            Err(GuardianInputTransactionError::Protocol(
                GuardianProtocolError::EffectIdentityConflict
            ))
        ));
        assert_eq!(state, before_payload_splice);

        let query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(63),
            Some(pane),
            1,
            0,
            Some(effect),
            &input_effect_query_payload(&input),
        );
        assert_eq!(
            apply_request(&mut state, &query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::DurablePrefix { applied_bytes: 3 },
            }
        );

        let mut exited_state = state.clone();
        exited_state.mark_exited(pane, 0).unwrap();
        let before_wrong_mux_query = exited_state.clone();
        let wrong_mux_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            id(90),
            id(66),
            Some(pane),
            1,
            0,
            Some(effect),
            &input_effect_query_payload(&input),
        );
        assert_eq!(
            apply_request(&mut exited_state, &wrong_mux_query),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch),
            "a retained disposition remains bound to its authenticated mux incarnation"
        );
        assert_eq!(exited_state, before_wrong_mux_query);

        let before_wrong_length_query = state.clone();
        let wrong_length_query_payload = GuardianInputEffectQuery::new(
            identity.sequence(),
            identity.input_bytes() - 1,
            identity.payload_sha256(),
        )
        .unwrap()
        .encode();
        let wrong_length_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(64),
            Some(pane),
            1,
            0,
            Some(effect),
            &wrong_length_query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &wrong_length_query),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch)
        );
        assert_eq!(state, before_wrong_length_query);

        let response = GuardianResponseEnvelope::success(&authenticated_input, &partial).unwrap();
        let encoded = encode_guardian_response(&secret(), &response).unwrap();
        let decoded = decode_guardian_response(&secret(), &encoded).unwrap();
        assert_eq!(decoded.success_reply(&authenticated_input).unwrap(), partial);
        let mut same_length_splice = encoded;
        let applied_offset = FRAME_LENGTH_BYTES + RESPONSE_FRAME_HEADER_BYTES + 49;
        same_length_splice[applied_offset..applied_offset + 4]
            .copy_from_slice(&2_u32.to_be_bytes());
        assert!(
            matches!(
                decode_guardian_response(&secret(), &same_length_splice),
                Err(GuardianProtocolError::AuthenticationFailed)
            ),
            "the exact applied byte count must be covered by the response MAC"
        );
        assert_eq!(
            GuardianResponseEnvelope::success(
                &authenticated_input,
                &GuardianReply::InputReceipt {
                    pane_id: pane,
                    generation: 1,
                    sequence: 1,
                    effect_id: effect,
                    state: InputEffectState::DurablePrefix { applied_bytes: 6 },
                },
            ),
            Err(GuardianProtocolError::InvalidInputDisposition)
        );
        assert_eq!(
            GuardianReply::InputEffect {
                effect_id: effect,
                state: InputEffectState::DurablePrefix { applied_bytes: 0 },
            }
            .encode_for_operation(GuardianOperation::QueryInputEffect),
            Err(GuardianProtocolError::InvalidReplyPayload)
        );
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
            &resize_payload(24, 80),
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
            &resize_payload(24, 80),
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
            &GuardianSignal::Terminate.encode(),
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
            state
                .mark_input_durable_full(input_effect_identity(&input))
                .unwrap(),
            GuardianReply::InputReceipt {
                state: InputEffectState::DurableFull,
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
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &close),
            Err(GuardianProtocolError::InputDurabilityPending)
        );
        state
            .mark_input_durable_full(input_effect_identity(&input))
            .unwrap();
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
    fn input_reconciliation_preflights_reverse_index_and_every_request_alias() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let effect = id(9);
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
            id(8),
            Some(pane),
            1,
            1,
            Some(effect),
            b"reconciliation-preflight",
        );
        let pending_reply = apply_request(&mut state, &input).unwrap();
        let mut alias = input.clone();
        alias.header.request_id = id(10);
        assert_eq!(apply_request(&mut state, &alias).unwrap(), pending_reply);
        let identity = input_effect_identity(&input);

        let reverse_index = state.effect_request_ids.remove(&effect).unwrap();
        let missing_reverse_index = state.clone();
        assert_eq!(
            state.mark_input_durable_full(identity),
            Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-reverse-index"
            ))
        );
        assert_eq!(state, missing_reverse_index);
        state.effect_request_ids.insert(effect, reverse_index);

        let removed_alias = state.requests.remove(&id(10)).unwrap();
        let missing_alias = state.clone();
        assert_eq!(
            state.mark_input_durable_full(identity),
            Err(GuardianProtocolError::StateInvariantViolation(
                "input-reconciliation-request-alias"
            ))
        );
        assert_eq!(state, missing_alias);
        state.requests.insert(id(10), removed_alias);

        let durable = state.mark_input_durable_full(identity).unwrap();
        assert!(matches!(
            durable,
            GuardianReply::InputReceipt {
                state: InputEffectState::DurableFull,
                ..
            }
        ));
        for request_id in [id(8), id(10)] {
            assert_eq!(&state.requests.get(&request_id).unwrap().reply, &durable);
        }
        assert!(matches!(
            state.pane_state(pane),
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect: None,
                ..
            })
        ));
    }

    #[test]
    fn definitively_unapplied_input_is_distinct_and_exact_retries_are_inert() {
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
        let known_not_applied = state
            .mark_input_known_not_applied(input_effect_identity(&input))
            .unwrap();
        assert_eq!(
            known_not_applied,
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::KnownNotApplied,
            }
        );
        assert_eq!(
            apply_request(&mut state, &input).unwrap(),
            known_not_applied,
            "a known-zero effect retry must return its original receipt without reapplying input"
        );
        assert_eq!(
            state.mark_input_durable_full(input_effect_identity(&input)),
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
            &resize_payload(30, 100),
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
                &resize_payload(40, 120),
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
    fn effect_eviction_removes_newer_request_aliases_as_one_identity_unit() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let mut state = GuardianProtocolState::new_with_receipt_capacity(guardian, 4).unwrap();
        apply_request(&mut state, &spawn_request(guardian, mux, pane)).unwrap();
        apply_request(
            &mut state,
            &claim_request(guardian, mux, pane, 0, 6, 7),
        )
        .unwrap();

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
                &resize_payload(40, 120),
            )
        };
        let first = resize(8, 9, 1);
        apply_request(&mut state, &first).unwrap();
        apply_request(&mut state, &resize(10, 11, 2)).unwrap();

        let mut newer_first_alias = first.clone();
        newer_first_alias.header.request_id = id(12);
        assert_eq!(
            apply_request(&mut state, &newer_first_alias).unwrap(),
            apply_request(&mut state, &first).unwrap()
        );
        apply_request(&mut state, &resize(13, 14, 3)).unwrap();
        apply_request(&mut state, &resize(15, 16, 4)).unwrap();

        assert!(!state.effects.contains_key(&id(9)));
        assert!(!state.requests.contains_key(&newer_first_alias.header.request_id));
        assert!(!state
            .effect_request_ids
            .values()
            .any(|request_ids| request_ids.contains(&newer_first_alias.header.request_id)));
        assert!(!state
            .transient_request_order
            .contains(&newer_first_alias.header.request_id));

        let reused_effect_identity = resize(17, 9, 5);
        apply_request(&mut state, &reused_effect_identity).unwrap();
        assert_eq!(
            apply_request(&mut state, &newer_first_alias),
            Err(GuardianProtocolError::EffectIdentityConflict)
        );
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

        let second_spawn_payload = spawn_payload("second-bounded-command");
        let second_spawn = request(
            GuardianOperation::Spawn,
            guardian,
            mux,
            id(10),
            Some(second_pane),
            0,
            0,
            Some(id(11)),
            &second_spawn_payload,
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
                &resize_payload(28, 90),
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

        state
            .mark_input_durable_prefix(input_effect_identity(&input), 5)
            .unwrap();
        assert!(state.transient_effect_order.contains(&input_effect));
        apply_request(&mut state, &resize(18, 19, 3)).unwrap();
        assert!(state.effects.contains_key(&input_effect));
        apply_request(&mut state, &resize(20, 21, 4)).unwrap();
        assert!(
            !state.effects.contains_key(&input_effect),
            "a resolved input may rotate only after its sequence fence is durable"
        );
        let evicted_query_payload = input_effect_query_payload(&input);
        let evicted_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(31),
            Some(first_pane),
            1,
            0,
            Some(input_effect),
            &evicted_query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &evicted_query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: input_effect,
                state: InputEffectState::DispositionUnavailable,
            },
            "an evicted receipt below the durable sequence fence must never masquerade as safe to resend"
        );
        let stale_retry_callback = std::cell::Cell::new(false);
        assert!(matches!(
            state.apply_input_effect_transactionally(&authenticate(&input), |_| {
                stale_retry_callback.set(true);
                Ok::<(), std::convert::Infallible>(())
            }),
            Err(GuardianEffectTransactionError::Protocol(
                GuardianProtocolError::RepeatedSequence {
                    expected: 2,
                    observed: 1,
                }
            ))
        ));
        assert!(
            !stale_retry_callback.get(),
            "receipt eviction must never permit a known prefix to be applied again"
        );

        let stale_durability_identity = input_effect_identity(&input);
        let reused_input = request(
            GuardianOperation::Input,
            guardian,
            mux,
            id(22),
            Some(first_pane),
            1,
            2,
            Some(input_effect),
            b"later-input-reusing-rotated-effect-uuid",
        );
        assert!(matches!(
            apply_request(&mut state, &reused_input).unwrap(),
            GuardianReply::InputReceipt {
                pane_id,
                generation: 1,
                sequence: 2,
                effect_id,
                state: InputEffectState::AcceptedNotDurable,
            } if pane_id == first_pane && effect_id == input_effect
        ));
        let stale_query_payload = input_effect_query_payload(&input);
        let stale_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(23),
            Some(first_pane),
            1,
            0,
            Some(input_effect),
            &stale_query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &stale_query),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch),
            "a delayed query must not observe the disposition of a newer input that reused a rotated effect UUID"
        );
        let current_query_payload = input_effect_query_payload(&reused_input);
        let current_query = request(
            GuardianOperation::QueryInputEffect,
            guardian,
            mux,
            id(24),
            Some(first_pane),
            1,
            0,
            Some(input_effect),
            &current_query_payload,
        );
        assert_eq!(
            apply_request(&mut state, &current_query).unwrap(),
            GuardianReply::InputEffect {
                effect_id: input_effect,
                state: InputEffectState::AcceptedNotDurable,
            }
        );
        assert_eq!(
            GuardianReply::InputReceipt {
                pane_id: first_pane,
                generation: 1,
                sequence: 1,
                effect_id: input_effect,
                state: InputEffectState::DispositionUnavailable,
            }
            .encode_for_operation(GuardianOperation::Input),
            Err(GuardianProtocolError::InvalidReplyPayload),
            "receipt-window uncertainty is a query result, never an acknowledgement of a newly accepted input"
        );
        assert_eq!(
            state.mark_input_durable_prefix(stale_durability_identity, 5),
            Err(GuardianProtocolError::InputDurabilityIdentityMismatch),
            "a delayed journal acknowledgement must not complete a newer input that reused a rotated effect UUID"
        );
        assert!(matches!(
            state.pane_state(first_pane),
            Some(GuardianPaneState::LiveClaimed {
                pending_input_effect: Some(effect),
                ..
            }) if *effect == input_effect
        ));
        assert!(matches!(
            state
                .mark_input_durable_full(input_effect_identity(&reused_input))
                .unwrap(),
            GuardianReply::InputReceipt {
                sequence: 2,
                state: InputEffectState::DurableFull,
                ..
            }
        ));
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
        let mut generation_state =
            GuardianProtocolState::new_with_receipt_capacity(guardian, 1).unwrap();
        apply_request(
            &mut generation_state,
            &spawn_request(guardian, mux, pane),
        )
        .unwrap();
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
