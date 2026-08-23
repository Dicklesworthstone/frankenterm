//! Authenticated, bounded protocol and pure fencing state machine for the PTY guardian.
//!
//! This module deliberately contains no sockets, PTYs, subprocesses, or mux-global lookups.
//! A transport must decode and authenticate a complete frame here before it is allowed to
//! route the request to a pane runtime.  The pure state machine is the authority for spawn
//! idempotency, lease generations, mutation sequencing, and ambiguous input reconciliation.
//! A fresh mux first uses the authenticated `Hello` operation to learn the current guardian
//! incarnation; nil incarnation scope is otherwise forbidden.

use hmac::{Hmac, KeyInit, Mac};
use portable_pty::{PtySize, cmdbuilder::CommandBuilder};
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
const SPAWN_PAYLOAD_MAGIC: [u8; 4] = *b"GSP1";
const RESIZE_PAYLOAD_MAGIC: [u8; 4] = *b"GRS1";
const SIGNAL_PAYLOAD_MAGIC: [u8; 4] = *b"GSG1";
const INPUT_EFFECT_QUERY_PAYLOAD_MAGIC: [u8; 4] = *b"GIQ1";
const REJECTION_PAYLOAD_MAGIC: [u8; 4] = *b"GRE1";
const SPAWN_PAYLOAD_FIXED_BYTES: usize = 16;
const RESIZE_PAYLOAD_BYTES: usize = 12;
const SIGNAL_PAYLOAD_BYTES: usize = 5;
const INPUT_EFFECT_QUERY_PAYLOAD_BYTES: usize = 44;
const REJECTION_PAYLOAD_BYTES: usize = 6;

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
}

impl GuardianRejectionCode {
    #[must_use]
    pub const fn status(self) -> GuardianResponseStatus {
        match self {
            Self::PaneNotFound
            | Self::SequenceGap
            | Self::CapacityExhausted
            | Self::RequestAliasCapacityExhausted
            | Self::InputDurabilityPending => GuardianResponseStatus::Rejected,
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
        if status == GuardianResponseStatus::Success
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
            _ => return Err(GuardianProtocolError::InvalidRejectionPayload),
        };
        if code.status() != status {
            return Err(GuardianProtocolError::InvalidRejectionPayload);
        }
        Ok(code)
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

    pub fn success(
        request: &AuthenticatedGuardianRequest,
        reply: &GuardianReply,
    ) -> Result<Self, GuardianProtocolError> {
        let payload = reply.encode_for_operation(request.header.operation)?;
        let response = Self {
            header: GuardianResponseHeader::new(
                &request.header,
                GuardianResponseStatus::Success,
                &payload,
            ),
            payload,
        };
        reply.require_response_identity(&response.header)?;
        reply.require_request_payload(request)?;
        Ok(response)
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
        payload.extend_from_slice(&command);
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

struct GuardianBoundedPayloadBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl GuardianBoundedPayloadBuffer {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            exceeded: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
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
    DurableEffect,
    TerminalRejected,
    DispositionUnavailable,
}

/// Exact authority for completing one authenticated input effect.
///
/// Effect UUIDs may be reused only after their bounded receipt rotates and a
/// later generation/sequence fence makes the old mutation impossible. Binding
/// runtime durability completion to the full authenticated fingerprint keeps a
/// delayed journal acknowledgement for that old UUID from completing a newer
/// input that happens to reuse it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianInputEffectIdentity {
    pane_id: Uuid,
    mux_incarnation: Uuid,
    generation: u64,
    sequence: u64,
    effect_id: Uuid,
    payload_sha256: [u8; 32],
}

impl GuardianInputEffectIdentity {
    pub fn from_authenticated_request(
        request: &AuthenticatedGuardianRequest,
    ) -> Result<Self, GuardianProtocolError> {
        validate_request_envelope(request)?;
        if request.header.operation != GuardianOperation::Input {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
        Ok(Self {
            pane_id: request
                .header
                .pane_id
                .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?,
            mux_incarnation: request.header.mux_incarnation,
            generation: request.header.lease_generation,
            sequence: request.header.lease_sequence,
            effect_id: request
                .header
                .effect_id
                .ok_or(GuardianProtocolError::InputDurabilityIdentityMismatch)?,
            payload_sha256: request.header.payload_sha256,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianInputEffectQuery {
    sequence: u64,
    payload_sha256: [u8; 32],
}

impl GuardianInputEffectQuery {
    pub fn new(
        sequence: u64,
        payload_sha256: [u8; 32],
    ) -> Result<Self, GuardianProtocolError> {
        if sequence == 0 {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        Ok(Self {
            sequence,
            payload_sha256,
        })
    }

    #[must_use]
    pub fn encode(self) -> [u8; INPUT_EFFECT_QUERY_PAYLOAD_BYTES] {
        let mut payload = [0_u8; INPUT_EFFECT_QUERY_PAYLOAD_BYTES];
        payload[..4].copy_from_slice(&INPUT_EFFECT_QUERY_PAYLOAD_MAGIC);
        payload[4..12].copy_from_slice(&self.sequence.to_be_bytes());
        payload[12..].copy_from_slice(&self.payload_sha256);
        payload
    }

    pub fn decode(payload: &[u8]) -> Result<Self, GuardianProtocolError> {
        if payload.len() != INPUT_EFFECT_QUERY_PAYLOAD_BYTES
            || payload[..4] != INPUT_EFFECT_QUERY_PAYLOAD_MAGIC
        {
            return Err(GuardianProtocolError::InvalidOperationPayload);
        }
        let mut payload_sha256 = [0_u8; 32];
        payload_sha256.copy_from_slice(&payload[12..]);
        Self::new(read_u64(payload, 4)?, payload_sha256)
    }
}

impl InputEffectState {
    const fn to_wire(self) -> u8 {
        match self {
            Self::NotSeen => 0,
            Self::AcceptedNotDurable => 1,
            Self::DurableEffect => 2,
            Self::TerminalRejected => 3,
            Self::DispositionUnavailable => 4,
        }
    }

    fn from_wire(value: u8) -> Result<Self, GuardianProtocolError> {
        match value {
            0 => Ok(Self::NotSeen),
            1 => Ok(Self::AcceptedNotDurable),
            2 => Ok(Self::DurableEffect),
            3 => Ok(Self::TerminalRejected),
            4 => Ok(Self::DispositionUnavailable),
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
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
    pub exit_status: Option<i32>,
    pub quarantine_reason: Option<GuardianQuarantineReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuardianReply {
    Hello {
        guardian_incarnation: Uuid,
    },
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

impl GuardianReply {
    pub fn encode_for_operation(
        &self,
        operation: GuardianOperation,
    ) -> Result<Vec<u8>, GuardianProtocolError> {
        self.require_operation(operation)?;
        let capacity = match self {
            Self::Hello { .. } => 16,
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
            Self::InputReceipt { .. } => 49,
            Self::InputEffect { .. } => 17,
        };
        if capacity > GUARDIAN_MAX_PAYLOAD_BYTES {
            return Err(GuardianProtocolError::PayloadTooLarge);
        }
        let mut payload = Vec::with_capacity(capacity);
        match self {
            Self::Hello {
                guardian_incarnation,
            } => push_uuid(&mut payload, *guardian_incarnation),
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
                payload.push(state.to_wire());
            }
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
                payload.push(state.to_wire());
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
        let reply = match operation {
            GuardianOperation::Hello => {
                require_reply_len(payload, 16)?;
                Self::Hello {
                    guardian_incarnation: read_required_uuid(payload, 0)?,
                }
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
                require_reply_len(payload, 49)?;
                Self::InputReceipt {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                    sequence: read_u64(payload, 24)?,
                    effect_id: read_required_uuid(payload, 32)?,
                    state: InputEffectState::from_wire(payload[48])?,
                }
            }
            GuardianOperation::Resize
            | GuardianOperation::Signal
            | GuardianOperation::Close
            | GuardianOperation::Checkpoint => {
                require_reply_len(payload, 32)?;
                Self::MutationApplied {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                    sequence: read_u64(payload, 24)?,
                }
            }
            GuardianOperation::Replay => {
                require_reply_len(payload, 24)?;
                Self::ReplayReady {
                    pane_id: read_required_uuid(payload, 0)?,
                    generation: read_u64(payload, 16)?,
                }
            }
            GuardianOperation::QueryInputEffect => {
                require_reply_len(payload, 17)?;
                Self::InputEffect {
                    effect_id: read_required_uuid(payload, 0)?,
                    state: InputEffectState::from_wire(payload[16])?,
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
                | (GuardianOperation::Spawn, Self::Spawned { .. })
                | (GuardianOperation::Census, Self::CensusPage { .. })
                | (GuardianOperation::Claim, Self::Claimed { .. })
                | (GuardianOperation::Attach, Self::Attached { .. })
                | (GuardianOperation::Input, Self::InputReceipt { .. })
                | (
                    GuardianOperation::Resize
                        | GuardianOperation::Signal
                        | GuardianOperation::Close
                        | GuardianOperation::Checkpoint,
                    Self::MutationApplied { .. }
                )
                | (GuardianOperation::Replay, Self::ReplayReady { .. })
                | (
                    GuardianOperation::QueryInputEffect,
                    Self::InputEffect { .. }
                )
                | (GuardianOperation::RetireLease, Self::LeaseRetired { .. })
        );
        if matches {
            let valid = match self {
                Self::Hello {
                    guardian_incarnation,
                } => !guardian_incarnation.is_nil(),
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
                        && matches!(
                            state,
                            InputEffectState::AcceptedNotDurable
                                | InputEffectState::DurableEffect
                                | InputEffectState::TerminalRejected
                        )
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
                Self::InputEffect { effect_id, .. } => !effect_id.is_nil(),
                Self::ReplayReady { pane_id, .. } => !pane_id.is_nil(),
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
        let Self::CensusPage {
            snapshot_id,
            entries,
            next_cursor,
            total_panes,
        } = self
        else {
            return Ok(());
        };
        let page = GuardianCensusPageRequest::decode(&request.payload)?;
        let entry_count =
            u64::try_from(entries.len()).map_err(|_| GuardianProtocolError::InvalidReplyPayload)?;
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

    fn encode_into(&self, payload: &mut Vec<u8>) -> Result<(), GuardianProtocolError> {
        self.validate_wire_shape()?;
        let start = payload.len();
        push_uuid(payload, self.pane_id);
        payload.push(self.status.to_wire());
        payload.extend_from_slice(&self.generation.to_be_bytes());
        push_optional_uuid(payload, self.mux_incarnation);
        payload.extend_from_slice(&self.next_sequence.unwrap_or(0).to_be_bytes());
        push_optional_uuid(payload, self.pending_input_effect);
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
        let flags = payload[70];
        if flags & !1 != 0 {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let encoded_exit_status = read_i32(payload, 65)?;
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
            exit_status,
            quarantine_reason: GuardianQuarantineReason::from_wire(payload[69])?,
        };
        entry.validate_wire_shape()?;
        Ok(entry)
    }

    fn validate_wire_shape(&self) -> Result<(), GuardianProtocolError> {
        if self.pane_id.is_nil()
            || self.next_sequence == Some(0)
            || self.mux_incarnation.is_some_and(|value| value.is_nil())
            || self.pending_input_effect.is_some_and(|value| value.is_nil())
        {
            return Err(GuardianProtocolError::InvalidReplyPayload);
        }
        let valid = match self.status {
            GuardianCensusPaneStatus::LiveUnclaimed => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.pending_input_effect.is_none()
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
                    && self.quarantine_reason.is_none()
            }
            GuardianCensusPaneStatus::Quarantined => {
                self.mux_incarnation.is_none()
                    && self.next_sequence.is_none()
                    && self.pending_input_effect.is_none()
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
        }
    }

    fn from_wire(value: u8) -> Result<Option<Self>, GuardianProtocolError> {
        match value {
            0 => Ok(None),
            1 => Ok(Some(Self::GenerationExhausted)),
            2 => Ok(Some(Self::SequenceExhausted)),
            _ => Err(GuardianProtocolError::InvalidReplyPayload),
        }
    }
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
    #[error("guardian success reply payload is malformed or violates its operation schema")]
    InvalidReplyPayload,
    #[error("guardian reply variant does not match operation {operation:?}")]
    ReplyOperationMismatch { operation: GuardianOperation },
    #[error("guardian correlated response is not a success reply")]
    NonSuccessResponse,
    #[error("guardian rejection payload is malformed or disagrees with its response status")]
    InvalidRejectionPayload,
    #[error("guardian operation payload is malformed or violates its frozen schema")]
    InvalidOperationPayload,
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
            | GuardianProtocolError::InvalidCensusPage
            | GuardianProtocolError::InvalidReplyPayload
            | GuardianProtocolError::ReplyOperationMismatch { .. }
            | GuardianProtocolError::NonSuccessResponse
            | GuardianProtocolError::InvalidRejectionPayload
            | GuardianProtocolError::InvalidOperationPayload => Self::InvalidRequest,
        }
    }
}

#[derive(Debug)]
pub enum GuardianEffectTransactionError<E> {
    Protocol(GuardianProtocolError),
    Effect(E),
}

impl<E> From<GuardianProtocolError> for GuardianEffectTransactionError<E> {
    fn from(error: GuardianProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl<E: std::fmt::Display> std::fmt::Display for GuardianEffectTransactionError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => std::fmt::Display::fmt(error, formatter),
            Self::Effect(error) => write!(formatter, "guardian runtime effect failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for GuardianEffectTransactionError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(error) => Some(error),
            Self::Effect(error) => Some(error),
        }
    }
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

#[derive(Debug, Default)]
struct ReceiptCapacityPlan {
    request_queue_pops: usize,
    request_ids: Vec<Uuid>,
    effect_queue_pops: usize,
    effect_ids: Vec<Uuid>,
}

#[derive(Debug)]
pub struct GuardianProtocolState {
    incarnation: Uuid,
    panes: BTreeMap<Uuid, GuardianPaneState>,
    census_snapshots: HashMap<Uuid, GuardianCensusSnapshot>,
    census_snapshot_order: VecDeque<Uuid>,
    next_census_snapshot_sequence: u128,
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
            next_census_snapshot_sequence: 1,
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

    /// Apply an authenticated operation that cannot create or mutate a runtime effect.
    ///
    /// Effect-producing requests must use [`Self::apply_effect_transactionally`]. Keeping
    /// the surfaces separate prevents a transport from advancing a lease or recording a
    /// spawn before the corresponding PTY/process operation has actually succeeded.
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
    /// and its new receipts are committed only after the callback returns `Ok(())`. Exhausted
    /// generation/sequence counters are the deliberate exception: preflight rejects the effect
    /// and terminally quarantines the pane so wrapped authority can never be revived.
    ///
    /// A callback error MUST mean that the runtime effect was not externally observable.
    /// In particular, an input write that may have written any bytes must return `Ok(())` so
    /// the protocol records `AcceptedNotDurable`; the runtime must then reconcile that exact
    /// effect through `mark_input_durable` or `mark_input_terminal_rejected`. Blindly treating
    /// a partial/ambiguous input write as `Err` would make retry duplication possible.
    pub fn apply_effect_transactionally<E>(
        &mut self,
        request: &AuthenticatedGuardianRequest,
        perform_effect: impl FnOnce(&GuardianReply) -> Result<(), E>,
    ) -> Result<GuardianReply, GuardianEffectTransactionError<E>> {
        validate_request_envelope(request)?;
        if request.header.guardian_incarnation != self.incarnation {
            return Err(GuardianProtocolError::GuardianIncarnationMismatch.into());
        }
        if !request.header.operation.creates_effect() {
            return Err(GuardianProtocolError::InvalidOperationScope {
                operation: request.header.operation,
            }
            .into());
        }
        self.apply_effect_transaction_inner(request, perform_effect)
    }

    #[cfg(test)]
    fn apply(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        if request.header.operation.creates_effect() {
            match self.apply_effect_transactionally(request, |_| Ok::<(), std::convert::Infallible>(()))) {
                Ok(reply) => Ok(reply),
                Err(GuardianEffectTransactionError::Protocol(error)) => Err(error),
                Err(GuardianEffectTransactionError::Effect(never)) => match never {},
            }
        } else {
            self.apply_observation(request)
        }
    }

    pub fn mark_input_durable(
        &mut self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(identity, InputEffectState::DurableEffect)
    }

    pub fn mark_input_terminal_rejected(
        &mut self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.transition_pending_input(identity, InputEffectState::TerminalRejected)
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
            InputEffectState::DurableEffect | InputEffectState::TerminalRejected
        ) {
            return Err(GuardianProtocolError::InputDurabilityIdentityMismatch);
        }
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
            || stored.fingerprint.payload_sha256 != identity.payload_sha256
        {
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
        let query = GuardianInputEffectQuery::decode(&request.payload)?;
        let state = match self.effects.get(&effect_id) {
            None => self.missing_input_effect_state(pane_id, query.sequence)?,
            Some(stored)
                if stored.fingerprint.pane_id == pane_id
                    && stored.fingerprint.operation == GuardianOperation::Input
                    && stored.fingerprint.lease_generation == request.header.lease_generation
                    && stored.fingerprint.lease_sequence == query.sequence
                    && stored.fingerprint.payload_sha256 == query.payload_sha256 =>
            {
                stored.state
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
        perform_effect: impl FnOnce(&GuardianReply) -> Result<(), E>,
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
            return Err(GuardianProtocolError::RequestIdentityConflict.into());
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
                    }
                    .into());
                }
                let capacity = self.plan_receipt_capacity(true, false)?;
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
                    .entry(effect_id)
                    .or_default()
                    .insert(request.header.request_id);
                if !disposition_is_pending {
                    self.transient_request_order
                        .push_back(request.header.request_id);
                }
                return Ok(reply);
            }
            return Err(GuardianProtocolError::EffectIdentityConflict.into());
        }
        let (reply, next_pane_state) =
            self.plan_new_effect(pane_id, effect_id, request)?;
        let capacity = self.plan_receipt_capacity(true, true)?;

        perform_effect(&reply).map_err(GuardianEffectTransactionError::Effect)?;

        // The exact eviction set was proven before the callback, but historical receipts
        // remain untouched until the runtime effect succeeds. Committing the precomputed
        // plan cannot fail, so a callback error leaves every protocol map and queue intact.
        self.commit_receipt_capacity(capacity);
        self.panes.insert(pane_id, next_pane_state);
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
            | GuardianOperation::Signal
            | GuardianOperation::Checkpoint => {
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
    if header.status == GuardianResponseStatus::Success {
        let reply = GuardianReply::decode_for_operation(header.operation, &response.payload)?;
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
    let pane_required = !matches!(operation, GuardianOperation::Census | GuardianOperation::Hello);
    let lease_required = operation.requires_lease();
    let effect_required =
        operation.creates_effect() || operation == GuardianOperation::QueryInputEffect;
    let spawn_scope_ok = operation != GuardianOperation::Spawn
        || (lease_generation == 0 && lease_sequence == 0);
    let observation_scope_ok = !matches!(operation, GuardianOperation::Census | GuardianOperation::Hello)
        || (pane_id.is_none()
            && effect_id.is_none()
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
        GuardianOperation::Hello
        | GuardianOperation::Claim
        | GuardianOperation::Attach
        | GuardianOperation::Close
        | GuardianOperation::Checkpoint
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

        let query = GuardianInputEffectQuery::new(7, [0x3c; 32]).unwrap();
        assert_eq!(
            GuardianInputEffectQuery::decode(&query.encode()).unwrap(),
            query
        );
        assert_eq!(
            GuardianInputEffectQuery::new(0, [0x3c; 32]),
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
    fn payload_free_operations_reject_authenticated_trailing_bytes() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        for (index, operation) in [
            GuardianOperation::Claim,
            GuardianOperation::Attach,
            GuardianOperation::Close,
            GuardianOperation::Checkpoint,
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
    fn failed_runtime_effect_does_not_publish_spawn_or_consume_replay_identity() {
        let guardian = id(1);
        let mux = id(2);
        let pane = id(3);
        let request = authenticate(&spawn_request(guardian, mux, pane));
        let mut state = GuardianProtocolState::new(guardian).unwrap();
        let invocations = std::cell::Cell::new(0usize);

        let failed = state.apply_effect_transactionally(&request, |_| {
            invocations.set(invocations.get() + 1);
            Err("injected spawn failure")
        });
        assert!(matches!(
            failed,
            Err(GuardianEffectTransactionError::Effect("injected spawn failure"))
        ));
        assert_eq!(invocations.get(), 1);
        assert_eq!(state.pane_state(pane), None);

        let first = state
            .apply_effect_transactionally(&request, |_| {
                invocations.set(invocations.get() + 1);
                Ok::<(), &str>(())
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
                Ok::<(), &str>(())
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
                Ok::<(), &str>(())
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
            state.apply_effect_transactionally(&resize, |_| Err("injected resize failure")),
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
                Ok::<(), &str>(())
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

        let failed = state.apply_effect_transactionally(&input, |_| Err("zero-byte write"));
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
            .apply_effect_transactionally(&input, |_| Ok::<(), &str>(()))
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
        let reply = GuardianReply::Spawned {
            pane_id: id(3),
            generation: 0,
        };
        let authenticated_request = authenticate(&original_request);
        let response = GuardianResponseEnvelope::success(&authenticated_request, &reply).unwrap();
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
            + 65;
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
            usize::try_from(GUARDIAN_CENSUS_PAGE_HEADER_BYTES).unwrap() + 70;
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
                .mark_input_durable(input_effect_identity(&input))
                .unwrap(),
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
                .mark_input_durable(input_effect_identity(&input))
                .unwrap(),
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
            b"",
        );
        assert_eq!(
            apply_request(&mut state, &close),
            Err(GuardianProtocolError::InputDurabilityPending)
        );
        state
            .mark_input_durable(input_effect_identity(&input))
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
            state
                .mark_input_terminal_rejected(input_effect_identity(&input))
                .unwrap(),
            GuardianReply::InputReceipt {
                pane_id: pane,
                generation: 1,
                sequence: 1,
                effect_id: effect,
                state: InputEffectState::TerminalRejected,
            }
        );
        assert_eq!(
            state.mark_input_durable(input_effect_identity(&input)),
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
            .mark_input_durable(input_effect_identity(&input))
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
        assert_eq!(
            apply_request(&mut state, &input),
            Err(GuardianProtocolError::RepeatedSequence {
                expected: 2,
                observed: 1,
            }),
            "receipt eviction must never permit a second input effect"
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
            state.mark_input_durable(stale_durability_identity),
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
                .mark_input_durable(input_effect_identity(&reused_input))
                .unwrap(),
            GuardianReply::InputReceipt {
                sequence: 2,
                state: InputEffectState::DurableEffect,
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
