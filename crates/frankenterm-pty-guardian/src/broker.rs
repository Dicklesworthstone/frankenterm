//! Production-disabled PTY broker typestate foundation.
//!
//! This module models the process-local ownership and authority transitions
//! needed by a future separately spawned broker process. The broker retains
//! the sole PTY master and exposes bounded authenticated proxy operations;
//! guardians never receive a master descriptor. That is essential because an
//! `SCM_RIGHTS` transfer cannot be revoked and socket EOF cannot fence a master
//! already installed in a predecessor guardian.
//!
//! There is deliberately no transport or command-line activation. This module
//! now contains the authenticated append-only Spawn WAL/head substrate for
//! Intent, Attempt, observed non-recycled child identity, Query, and reply Ack;
//! it also projects recovered records into the mux protocol's durable legacy
//! Spawn fence. The filesystem discovery/revalidation service and OS child
//! identity verifier are not wired yet, however, and the process-local PTY
//! typestate still does **not** survive guardian `SIGKILL`. Catalog Genesis
//! admission below remains durable pre-Spawn intent, never proof that a child
//! exists. Activation additionally requires a separately spawned same-binary
//! broker that opens/reconciles this WAL before traffic, sole-master proxy
//! transport, and a real cross-process crash matrix. The WAL types model the
//! marker-before-spawn and spawn-success-before-Ack cuts without claiming that
//! the current service executes those recovery paths.
//!
//! The ordering enforced here is:
//!
//! 1. validate an authenticated guardian connection and the exact canonical
//!    Spawn reservation;
//! 2. open the PTY and reserve one broker-owned master/reader/writer proxy, but
//!    create no child;
//! 3. consume the synchronously durable Genesis pre-Spawn intent;
//! 4. synchronize the broker Spawn Attempt before invoking the one callback,
//!    then record the verified child identity and reply acknowledgement;
//! 5. process-locally issue one logical guardian lease;
//! 6. fence every proxy operation at admission and again immediately before
//!    effect, so rotation invalidates already-queued stale work;
//! 7. accept a successor only after authenticated connection EOF revoked the
//!    old logical proxy lease and an exact generation/build-fenced handoff.
//!
//! Recovered WAL state can now reconstruct the pure protocol Spawn fence, and
//! every generic protocol Spawn dispatch consults it. Production startup must
//! still enumerate every WAL, perform the pinned no-follow filesystem checks,
//! install all fences before accepting traffic, and represent broker-owned
//! panes in census; until then the activation selector remains hard-disabled.

#![allow(dead_code)] // Activation is intentionally held for the cross-process tranche.

use crate::SealedAtomicBuildIdentity;
use crate::output::GuardianPublishedGenesisAdmissionPermitV1;
use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable};
use mux::guardian_checkpoint::GuardianGenesisReservationIdentityV1;
use mux::guardian_protocol::{
    GUARDIAN_MAC_BYTES, GUARDIAN_MAX_PANES, GUARDIAN_MAX_PAYLOAD_BYTES,
    GuardianBrokerControlAuthenticatorV1, GuardianBrokerSpawnWalAuthenticatorV1,
    GuardianDurableSpawnFenceV1, GuardianSpawnPayload,
};
use nix::unistd::geteuid;
#[cfg(test)]
use portable_pty::ExitStatus;
use portable_pty::{Child, MasterPty, PollablePtyReader, PtyPair, PtySize, native_pty_system};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{ZeroizeOnDrop, Zeroizing};

const BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS: u32 = 1_024;
const BROKER_CATALOG_CHECKSUM_BYTES: usize = 32;
const BROKER_DEFAULT_MAX_PROXY_OPERATION_BYTES: usize = 64 * 1024;
const BROKER_DEFAULT_MAX_BUFFERED_OUTPUT_BYTES: usize = 1024 * 1024;
const BROKER_ABSOLUTE_MAX_BUFFERED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const BROKER_OUTPUT_PUMP_CHUNK_BYTES: usize = 8 * 1024;
const BROKER_SPAWN_WAL_FILE_MAGIC: [u8; 8] = *b"FTBSW001";
const BROKER_SPAWN_HEAD_FILE_MAGIC: [u8; 8] = *b"FTBSH001";
const BROKER_SPAWN_WAL_RECORD_MAGIC: [u8; 8] = *b"FTBSR001";
const BROKER_SPAWN_HEAD_RECORD_MAGIC: [u8; 8] = *b"FTBHR001";
const BROKER_SPAWN_WAL_FORMAT_VERSION: u32 = 1;
const BROKER_SPAWN_WAL_FILE_HEADER_BYTES: usize = 224;
const BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U32: u32 = 224;
const BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64: u64 = 224;
const BROKER_SPAWN_WAL_RECORD_BYTES: usize = 176;
const BROKER_SPAWN_WAL_RECORD_BYTES_U32: u32 = 176;
const BROKER_SPAWN_WAL_RECORD_BYTES_U64: u64 = 176;
const BROKER_SPAWN_HEAD_RECORD_BYTES: usize = 120;
const BROKER_SPAWN_HEAD_RECORD_BYTES_U32: u32 = 120;
const BROKER_SPAWN_HEAD_RECORD_BYTES_U64: u64 = 120;
const BROKER_SPAWN_WAL_AUTHENTICATED_HEADER_BYTES: usize = 192;
const BROKER_SPAWN_WAL_AUTHENTICATED_RECORD_BYTES: usize = 144;
const BROKER_SPAWN_HEAD_AUTHENTICATED_RECORD_BYTES: usize = 88;
const BROKER_SPAWN_WAL_MAC_BYTES: usize = 32;
const BROKER_SPAWN_WAL_KEY_ID_BYTES: usize = 8;
const BROKER_SPAWN_WAL_MAX_RECORDS: u64 = 4;
const BROKER_SPAWN_WAL_MAX_PHYSICAL_BYTES: u64 = BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
    + BROKER_SPAWN_WAL_MAX_RECORDS * BROKER_SPAWN_WAL_RECORD_BYTES_U64;
const BROKER_SPAWN_HEAD_MAX_PHYSICAL_BYTES: u64 = BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
    + BROKER_SPAWN_WAL_MAX_RECORDS * BROKER_SPAWN_HEAD_RECORD_BYTES_U64;
const BROKER_SPAWN_CATALOG_LOCK_NAME: &str = ".broker-spawn-catalog.lock.v1";
const BROKER_SPAWN_CATALOG_PREFIX: &str = "spawn-";
const BROKER_SPAWN_CATALOG_WAL_SUFFIX: &str = ".wal.v1";
const BROKER_SPAWN_CATALOG_HEAD_SUFFIX: &str = ".head.v1";
const BROKER_SPAWN_CATALOG_MAX_ENTRIES: usize = GUARDIAN_MAX_PANES * 2 + 1;
const BROKER_CONTROL_REQUEST_MAGIC: [u8; 4] = *b"FTBQ";
const BROKER_CONTROL_RESPONSE_MAGIC: [u8; 4] = *b"FTBP";
const BROKER_CONTROL_VERSION: u16 = 1;
const BROKER_CONTROL_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const BROKER_CONTROL_REQUEST_FIXED_BYTES: usize = 240;
const BROKER_CONTROL_REQUEST_PAYLOAD_OFFSET: usize = 208;
const BROKER_CONTROL_RESPONSE_FIXED_BYTES: usize = 228;
const BROKER_CONTROL_RESPONSE_PAYLOAD_OFFSET: usize = 196;
pub(crate) const BROKER_CONTROL_MAX_FRAME_BYTES: usize =
    BROKER_CONTROL_REQUEST_FIXED_BYTES + BROKER_CONTROL_MAX_PAYLOAD_BYTES;
const BROKER_GENESIS_BINDING_BYTES: usize = 256;
const BROKER_SPAWN_CONTROL_MAGIC: [u8; 4] = *b"BSP1";
const BROKER_SPAWN_CONTROL_FIXED_BYTES: usize = 332;

fn is_pty_terminal_eio(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
}

/// Fixed operation vocabulary for the guardian-to-broker control channel.
///
/// Every effect has an explicit query and acknowledgement path. Reads remain
/// replayable until `AcknowledgeOutput`; writes, resizes, Spawn, attachment,
/// and retirement retain content-free receipts until `AcknowledgeEffect`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokerControlOperationV1 {
    Hello = 1,
    Spawn = 2,
    QueryEffect = 3,
    AcknowledgeEffect = 4,
    Write = 5,
    Resize = 6,
    ReadOutput = 7,
    AcknowledgeOutput = 8,
    AttachSuccessor = 9,
    Census = 10,
    ClosePane = 11,
}

impl BrokerControlOperationV1 {
    fn from_wire(value: u8) -> Result<Self, BrokerControlProtocolError> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Spawn),
            3 => Ok(Self::QueryEffect),
            4 => Ok(Self::AcknowledgeEffect),
            5 => Ok(Self::Write),
            6 => Ok(Self::Resize),
            7 => Ok(Self::ReadOutput),
            8 => Ok(Self::AcknowledgeOutput),
            9 => Ok(Self::AttachSuccessor),
            10 => Ok(Self::Census),
            11 => Ok(Self::ClosePane),
            _ => Err(BrokerControlProtocolError::InvalidOperation),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokerControlResponseStatusV1 {
    Applied = 1,
    Recovered = 2,
    Rejected = 3,
    Retryable = 4,
    Quarantined = 5,
    Terminal = 6,
}

impl BrokerControlResponseStatusV1 {
    fn from_wire(value: u8) -> Result<Self, BrokerControlProtocolError> {
        match value {
            1 => Ok(Self::Applied),
            2 => Ok(Self::Recovered),
            3 => Ok(Self::Rejected),
            4 => Ok(Self::Retryable),
            5 => Ok(Self::Quarantined),
            6 => Ok(Self::Terminal),
            _ => Err(BrokerControlProtocolError::InvalidStatus),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrokerControlRequestHeaderV1 {
    pub operation: BrokerControlOperationV1,
    pub request_id: Uuid,
    pub broker_incarnation: Uuid,
    pub guardian_incarnation: Uuid,
    pub connection_id: Uuid,
    pub mux_incarnation: Uuid,
    pub guardian_build_identity_digest: [u8; 32],
    pub mux_build_identity_digest: [u8; 32],
    pub durable_pane_id: Uuid,
    pub lease_generation: u64,
    pub operation_id: Uuid,
}

impl BrokerControlRequestHeaderV1 {
    fn validate(self, payload_bytes: usize) -> Result<(), BrokerControlProtocolError> {
        if self.request_id.is_nil()
            || self.guardian_incarnation.is_nil()
            || self.connection_id.is_nil()
            || self.mux_incarnation.is_nil()
            || self.guardian_build_identity_digest == [0; 32]
            || self.mux_build_identity_digest == [0; 32]
            || payload_bytes > BROKER_CONTROL_MAX_PAYLOAD_BYTES
        {
            return Err(BrokerControlProtocolError::InvalidIdentity);
        }
        let valid = match self.operation {
            BrokerControlOperationV1::Hello => {
                self.broker_incarnation.is_nil()
                    && self.durable_pane_id.is_nil()
                    && self.lease_generation == 0
                    && self.operation_id.is_nil()
                    && payload_bytes == 0
            }
            BrokerControlOperationV1::Census => {
                !self.broker_incarnation.is_nil()
                    && self.durable_pane_id.is_nil()
                    && self.lease_generation == 0
                    && self.operation_id.is_nil()
                    && payload_bytes == 0
            }
            BrokerControlOperationV1::Spawn => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation == 0
                    && !self.operation_id.is_nil()
                    && payload_bytes > 0
            }
            BrokerControlOperationV1::QueryEffect | BrokerControlOperationV1::AcknowledgeEffect => {
                // Generation zero is reserved for querying or acknowledging
                // the pre-lease Spawn effect. Once a pane lease exists, the
                // exact nonzero generation fences every other effect receipt.
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && !self.operation_id.is_nil()
                    && payload_bytes == 0
            }
            BrokerControlOperationV1::AttachSuccessor | BrokerControlOperationV1::ClosePane => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation > 0
                    && !self.operation_id.is_nil()
                    && payload_bytes == 0
            }
            BrokerControlOperationV1::Write => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation > 0
                    && !self.operation_id.is_nil()
                    && payload_bytes > 0
            }
            BrokerControlOperationV1::Resize => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation > 0
                    && !self.operation_id.is_nil()
                    && payload_bytes == 8
            }
            BrokerControlOperationV1::ReadOutput => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation > 0
                    && !self.operation_id.is_nil()
                    && payload_bytes == 4
            }
            BrokerControlOperationV1::AcknowledgeOutput => {
                !self.broker_incarnation.is_nil()
                    && !self.durable_pane_id.is_nil()
                    && self.lease_generation > 0
                    && !self.operation_id.is_nil()
                    && payload_bytes == 8
            }
        };
        if valid {
            Ok(())
        } else {
            Err(BrokerControlProtocolError::InvalidShape)
        }
    }
}

/// Authenticated request owner. Plaintext-bearing payload bytes are wiped on
/// drop and the type intentionally has no `Clone` implementation.
pub(crate) struct BrokerControlRequestV1 {
    pub header: BrokerControlRequestHeaderV1,
    payload: Zeroizing<Vec<u8>>,
}

impl BrokerControlRequestV1 {
    pub(crate) fn new(
        header: BrokerControlRequestHeaderV1,
        payload: &[u8],
    ) -> Result<Self, BrokerControlProtocolError> {
        header.validate(payload.len())?;
        let mut owned = Zeroizing::new(Vec::new());
        owned
            .try_reserve_exact(payload.len())
            .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
        owned.extend_from_slice(payload);
        Ok(Self {
            header,
            payload: owned,
        })
    }

    #[must_use]
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl std::fmt::Debug for BrokerControlRequestV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerControlRequestV1")
            .field("header", &self.header)
            .field("payload_bytes", &self.payload.len())
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BrokerControlResponseHeaderV1 {
    pub operation: BrokerControlOperationV1,
    pub status: BrokerControlResponseStatusV1,
    pub request_id: Uuid,
    pub broker_incarnation: Uuid,
    pub guardian_incarnation: Uuid,
    pub connection_id: Uuid,
    pub durable_pane_id: Uuid,
    pub lease_generation: u64,
    pub operation_id: Uuid,
    pub child_identity: Option<BrokerKernelChildIdentityV1>,
    pub output_sequence_start: u64,
    pub output_sequence_end: u64,
}

impl BrokerControlResponseHeaderV1 {
    fn validate(self, payload_bytes: usize) -> Result<(), BrokerControlProtocolError> {
        if self.request_id.is_nil()
            || self.broker_incarnation.is_nil()
            || self.guardian_incarnation.is_nil()
            || self.connection_id.is_nil()
            || payload_bytes > BROKER_CONTROL_MAX_PAYLOAD_BYTES
            || self.output_sequence_start > self.output_sequence_end
        {
            return Err(BrokerControlProtocolError::InvalidIdentity);
        }
        if let Some(child) = self.child_identity {
            child
                .validate()
                .map_err(|_| BrokerControlProtocolError::InvalidIdentity)?;
        }
        let pane_scoped = !self.durable_pane_id.is_nil() && !self.operation_id.is_nil();
        let global_scoped = self.durable_pane_id.is_nil()
            && self.lease_generation == 0
            && self.operation_id.is_nil();
        let empty_effect = self.child_identity.is_none()
            && self.output_sequence_start == 0
            && self.output_sequence_end == 0
            && payload_bytes == 0;
        let successful = matches!(
            self.status,
            BrokerControlResponseStatusV1::Applied | BrokerControlResponseStatusV1::Recovered
        );
        let unsuccessful = matches!(
            self.status,
            BrokerControlResponseStatusV1::Rejected
                | BrokerControlResponseStatusV1::Retryable
                | BrokerControlResponseStatusV1::Quarantined
        );
        let valid = match self.operation {
            BrokerControlOperationV1::Hello => {
                global_scoped && empty_effect && (successful || unsuccessful)
            }
            BrokerControlOperationV1::Census => {
                global_scoped
                    && self.child_identity.is_none()
                    && self.output_sequence_start == 0
                    && self.output_sequence_end == 0
                    && (successful || (unsuccessful && payload_bytes == 0))
            }
            BrokerControlOperationV1::Spawn => {
                pane_scoped
                    && self.output_sequence_start == 0
                    && self.output_sequence_end == 0
                    && payload_bytes == 0
                    && ((successful && self.lease_generation > 0 && self.child_identity.is_some())
                        || (unsuccessful
                            && self.lease_generation == 0
                            && self.child_identity.is_none()))
            }
            BrokerControlOperationV1::QueryEffect => {
                pane_scoped
                    && self.output_sequence_start == 0
                    && self.output_sequence_end == 0
                    && payload_bytes == 0
                    && ((successful
                        && ((self.lease_generation == 0 && self.child_identity.is_some())
                            || (self.lease_generation > 0 && self.child_identity.is_none())))
                        || (unsuccessful && self.child_identity.is_none()))
            }
            BrokerControlOperationV1::AcknowledgeEffect => {
                pane_scoped && empty_effect && (successful || unsuccessful)
            }
            BrokerControlOperationV1::Write
            | BrokerControlOperationV1::Resize
            | BrokerControlOperationV1::AcknowledgeOutput
            | BrokerControlOperationV1::AttachSuccessor => {
                pane_scoped
                    && self.lease_generation > 0
                    && empty_effect
                    && (successful || unsuccessful)
            }
            BrokerControlOperationV1::ClosePane => {
                pane_scoped
                    && self.lease_generation > 0
                    && empty_effect
                    && (successful
                        || unsuccessful
                        || self.status == BrokerControlResponseStatusV1::Terminal)
            }
            BrokerControlOperationV1::ReadOutput => {
                let output_bytes = self
                    .output_sequence_end
                    .checked_sub(self.output_sequence_start)
                    .and_then(|bytes| usize::try_from(bytes).ok());
                pane_scoped
                    && self.lease_generation > 0
                    && self.child_identity.is_none()
                    && ((successful && output_bytes == Some(payload_bytes))
                        || (unsuccessful
                            && self.output_sequence_start == 0
                            && self.output_sequence_end == 0
                            && payload_bytes == 0)
                        || (self.status == BrokerControlResponseStatusV1::Terminal
                            && output_bytes == Some(0)
                            && payload_bytes == 0))
            }
        };
        if valid {
            Ok(())
        } else {
            Err(BrokerControlProtocolError::InvalidShape)
        }
    }
}

pub(crate) struct BrokerControlResponseV1 {
    pub header: BrokerControlResponseHeaderV1,
    payload: Zeroizing<Vec<u8>>,
}

impl BrokerControlResponseV1 {
    pub(crate) fn new(
        header: BrokerControlResponseHeaderV1,
        payload: &[u8],
    ) -> Result<Self, BrokerControlProtocolError> {
        header.validate(payload.len())?;
        let mut owned = Zeroizing::new(Vec::new());
        owned
            .try_reserve_exact(payload.len())
            .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
        owned.extend_from_slice(payload);
        Ok(Self {
            header,
            payload: owned,
        })
    }

    #[must_use]
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl std::fmt::Debug for BrokerControlResponseV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerControlResponseV1")
            .field("header", &self.header)
            .field("payload_bytes", &self.payload.len())
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// Wipe-on-drop encoded broker-control frame.
pub(crate) struct BrokerControlWireFrameV1 {
    bytes: Zeroizing<Vec<u8>>,
}

impl BrokerControlWireFrameV1 {
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

impl ZeroizeOnDrop for BrokerControlWireFrameV1 {}

impl std::fmt::Debug for BrokerControlWireFrameV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerControlWireFrameV1")
            .field("frame_bytes", &self.bytes.len())
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub(crate) enum BrokerControlProtocolError {
    #[error("broker control frame exceeds its fixed resource bound")]
    CapacityExhausted,
    #[error("broker control frame is truncated or has a noncanonical length")]
    InvalidLength,
    #[error("broker control frame magic or version is invalid")]
    InvalidVersion,
    #[error("broker control operation is invalid")]
    InvalidOperation,
    #[error("broker control response status is invalid")]
    InvalidStatus,
    #[error("broker control frame identity is invalid")]
    InvalidIdentity,
    #[error("broker control frame shape is invalid for its operation")]
    InvalidShape,
    #[error("broker control authentication failed")]
    AuthenticationFailed,
}

pub(crate) fn encode_broker_control_request(
    authority: &GuardianBrokerControlAuthenticatorV1,
    request: &BrokerControlRequestV1,
) -> Result<BrokerControlWireFrameV1, BrokerControlProtocolError> {
    request.header.validate(request.payload.len())?;
    let total_bytes = BROKER_CONTROL_REQUEST_FIXED_BYTES
        .checked_add(request.payload.len())
        .ok_or(BrokerControlProtocolError::CapacityExhausted)?;
    if total_bytes > BROKER_CONTROL_MAX_FRAME_BYTES {
        return Err(BrokerControlProtocolError::CapacityExhausted);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    bytes
        .try_reserve_exact(total_bytes)
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes.resize(BROKER_CONTROL_REQUEST_PAYLOAD_OFFSET, 0);
    let body_bytes = u32::try_from(total_bytes - 4)
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes[0..4].copy_from_slice(&body_bytes.to_be_bytes());
    bytes[4..8].copy_from_slice(&BROKER_CONTROL_REQUEST_MAGIC);
    bytes[8..10].copy_from_slice(&BROKER_CONTROL_VERSION.to_be_bytes());
    bytes[10] = request.header.operation as u8;
    bytes[11] = 0;
    bytes[12..20].copy_from_slice(&authority.key_id());
    bytes[20..36].copy_from_slice(request.header.request_id.as_bytes());
    bytes[36..52].copy_from_slice(request.header.broker_incarnation.as_bytes());
    bytes[52..68].copy_from_slice(request.header.guardian_incarnation.as_bytes());
    bytes[68..84].copy_from_slice(request.header.connection_id.as_bytes());
    bytes[84..100].copy_from_slice(request.header.mux_incarnation.as_bytes());
    bytes[100..132].copy_from_slice(&request.header.guardian_build_identity_digest);
    bytes[132..164].copy_from_slice(&request.header.mux_build_identity_digest);
    bytes[164..180].copy_from_slice(request.header.durable_pane_id.as_bytes());
    bytes[180..188].copy_from_slice(&request.header.lease_generation.to_be_bytes());
    bytes[188..204].copy_from_slice(request.header.operation_id.as_bytes());
    let payload_bytes = u32::try_from(request.payload.len())
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes[204..208].copy_from_slice(&payload_bytes.to_be_bytes());
    bytes.extend_from_slice(&request.payload);
    let tag = authority
        .authenticate_request(&bytes)
        .map_err(|_| BrokerControlProtocolError::AuthenticationFailed)?;
    bytes.extend_from_slice(&tag);
    debug_assert_eq!(bytes.len(), total_bytes);
    Ok(BrokerControlWireFrameV1 { bytes })
}

pub(crate) fn decode_broker_control_request(
    authority: &GuardianBrokerControlAuthenticatorV1,
    frame: &[u8],
) -> Result<BrokerControlRequestV1, BrokerControlProtocolError> {
    validate_broker_control_frame_length(frame, BROKER_CONTROL_REQUEST_FIXED_BYTES)?;
    if frame[4..8] != BROKER_CONTROL_REQUEST_MAGIC
        || read_broker_be_u16(&frame[8..10]) != BROKER_CONTROL_VERSION
        || frame[11] != 0
        || frame[12..20] != authority.key_id()
    {
        return Err(BrokerControlProtocolError::InvalidVersion);
    }
    let payload_bytes = usize::try_from(read_broker_be_u32(&frame[204..208]))
        .map_err(|_| BrokerControlProtocolError::InvalidLength)?;
    let authenticated_bytes = BROKER_CONTROL_REQUEST_PAYLOAD_OFFSET
        .checked_add(payload_bytes)
        .ok_or(BrokerControlProtocolError::InvalidLength)?;
    if payload_bytes > BROKER_CONTROL_MAX_PAYLOAD_BYTES
        || authenticated_bytes
            .checked_add(GUARDIAN_MAC_BYTES)
            .ok_or(BrokerControlProtocolError::InvalidLength)?
            != frame.len()
    {
        return Err(BrokerControlProtocolError::InvalidLength);
    }
    authority
        .verify_request(&frame[..authenticated_bytes], &frame[authenticated_bytes..])
        .map_err(|_| BrokerControlProtocolError::AuthenticationFailed)?;
    BrokerControlRequestV1::new(
        BrokerControlRequestHeaderV1 {
            operation: BrokerControlOperationV1::from_wire(frame[10])?,
            request_id: read_broker_uuid(&frame[20..36]),
            broker_incarnation: read_broker_uuid(&frame[36..52]),
            guardian_incarnation: read_broker_uuid(&frame[52..68]),
            connection_id: read_broker_uuid(&frame[68..84]),
            mux_incarnation: read_broker_uuid(&frame[84..100]),
            guardian_build_identity_digest: read_broker_array_32(&frame[100..132]),
            mux_build_identity_digest: read_broker_array_32(&frame[132..164]),
            durable_pane_id: read_broker_uuid(&frame[164..180]),
            lease_generation: read_broker_be_u64(&frame[180..188]),
            operation_id: read_broker_uuid(&frame[188..204]),
        },
        &frame[BROKER_CONTROL_REQUEST_PAYLOAD_OFFSET..authenticated_bytes],
    )
}

pub(crate) fn encode_broker_control_response(
    authority: &GuardianBrokerControlAuthenticatorV1,
    response: &BrokerControlResponseV1,
) -> Result<BrokerControlWireFrameV1, BrokerControlProtocolError> {
    response.header.validate(response.payload.len())?;
    let total_bytes = BROKER_CONTROL_RESPONSE_FIXED_BYTES
        .checked_add(response.payload.len())
        .ok_or(BrokerControlProtocolError::CapacityExhausted)?;
    if total_bytes > BROKER_CONTROL_MAX_FRAME_BYTES {
        return Err(BrokerControlProtocolError::CapacityExhausted);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    bytes
        .try_reserve_exact(total_bytes)
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes.resize(BROKER_CONTROL_RESPONSE_PAYLOAD_OFFSET, 0);
    let body_bytes = u32::try_from(total_bytes - 4)
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes[0..4].copy_from_slice(&body_bytes.to_be_bytes());
    bytes[4..8].copy_from_slice(&BROKER_CONTROL_RESPONSE_MAGIC);
    bytes[8..10].copy_from_slice(&BROKER_CONTROL_VERSION.to_be_bytes());
    bytes[10] = response.header.operation as u8;
    bytes[11] = response.header.status as u8;
    bytes[12..20].copy_from_slice(&authority.key_id());
    bytes[20..36].copy_from_slice(response.header.request_id.as_bytes());
    bytes[36..52].copy_from_slice(response.header.broker_incarnation.as_bytes());
    bytes[52..68].copy_from_slice(response.header.guardian_incarnation.as_bytes());
    bytes[68..84].copy_from_slice(response.header.connection_id.as_bytes());
    bytes[84..100].copy_from_slice(response.header.durable_pane_id.as_bytes());
    bytes[100..108].copy_from_slice(&response.header.lease_generation.to_be_bytes());
    bytes[108..124].copy_from_slice(response.header.operation_id.as_bytes());
    if let Some(child) = response.header.child_identity {
        bytes[124..128].copy_from_slice(&child.process_id.to_be_bytes());
        bytes[128..144].copy_from_slice(child.broker_child_nonce.as_bytes());
        bytes[144..176].copy_from_slice(&child.kernel_start_identity_digest);
    }
    bytes[176..184].copy_from_slice(&response.header.output_sequence_start.to_be_bytes());
    bytes[184..192].copy_from_slice(&response.header.output_sequence_end.to_be_bytes());
    let payload_bytes = u32::try_from(response.payload.len())
        .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
    bytes[192..196].copy_from_slice(&payload_bytes.to_be_bytes());
    bytes.extend_from_slice(&response.payload);
    let tag = authority
        .authenticate_response(&bytes)
        .map_err(|_| BrokerControlProtocolError::AuthenticationFailed)?;
    bytes.extend_from_slice(&tag);
    debug_assert_eq!(bytes.len(), total_bytes);
    Ok(BrokerControlWireFrameV1 { bytes })
}

pub(crate) fn decode_broker_control_response(
    authority: &GuardianBrokerControlAuthenticatorV1,
    frame: &[u8],
) -> Result<BrokerControlResponseV1, BrokerControlProtocolError> {
    validate_broker_control_frame_length(frame, BROKER_CONTROL_RESPONSE_FIXED_BYTES)?;
    if frame[4..8] != BROKER_CONTROL_RESPONSE_MAGIC
        || read_broker_be_u16(&frame[8..10]) != BROKER_CONTROL_VERSION
        || frame[12..20] != authority.key_id()
    {
        return Err(BrokerControlProtocolError::InvalidVersion);
    }
    let payload_bytes = usize::try_from(read_broker_be_u32(&frame[192..196]))
        .map_err(|_| BrokerControlProtocolError::InvalidLength)?;
    let authenticated_bytes = BROKER_CONTROL_RESPONSE_PAYLOAD_OFFSET
        .checked_add(payload_bytes)
        .ok_or(BrokerControlProtocolError::InvalidLength)?;
    if payload_bytes > BROKER_CONTROL_MAX_PAYLOAD_BYTES
        || authenticated_bytes
            .checked_add(GUARDIAN_MAC_BYTES)
            .ok_or(BrokerControlProtocolError::InvalidLength)?
            != frame.len()
    {
        return Err(BrokerControlProtocolError::InvalidLength);
    }
    authority
        .verify_response(&frame[..authenticated_bytes], &frame[authenticated_bytes..])
        .map_err(|_| BrokerControlProtocolError::AuthenticationFailed)?;
    let process_id = read_broker_be_u32(&frame[124..128]);
    let broker_child_nonce = read_broker_uuid(&frame[128..144]);
    let kernel_start_identity_digest = read_broker_array_32(&frame[144..176]);
    let child_identity = if process_id == 0
        && broker_child_nonce.is_nil()
        && kernel_start_identity_digest == [0; 32]
    {
        None
    } else {
        Some(BrokerKernelChildIdentityV1 {
            process_id,
            broker_child_nonce,
            kernel_start_identity_digest,
        })
    };
    BrokerControlResponseV1::new(
        BrokerControlResponseHeaderV1 {
            operation: BrokerControlOperationV1::from_wire(frame[10])?,
            status: BrokerControlResponseStatusV1::from_wire(frame[11])?,
            request_id: read_broker_uuid(&frame[20..36]),
            broker_incarnation: read_broker_uuid(&frame[36..52]),
            guardian_incarnation: read_broker_uuid(&frame[52..68]),
            connection_id: read_broker_uuid(&frame[68..84]),
            durable_pane_id: read_broker_uuid(&frame[84..100]),
            lease_generation: read_broker_be_u64(&frame[100..108]),
            operation_id: read_broker_uuid(&frame[108..124]),
            child_identity,
            output_sequence_start: read_broker_be_u64(&frame[176..184]),
            output_sequence_end: read_broker_be_u64(&frame[184..192]),
        },
        &frame[BROKER_CONTROL_RESPONSE_PAYLOAD_OFFSET..authenticated_bytes],
    )
}

fn validate_broker_control_frame_length(
    frame: &[u8],
    minimum_bytes: usize,
) -> Result<(), BrokerControlProtocolError> {
    if frame.len() < minimum_bytes || frame.len() > BROKER_CONTROL_MAX_FRAME_BYTES {
        return Err(BrokerControlProtocolError::InvalidLength);
    }
    let announced = usize::try_from(read_broker_be_u32(&frame[..4]))
        .map_err(|_| BrokerControlProtocolError::InvalidLength)?;
    if announced.checked_add(4) != Some(frame.len()) {
        return Err(BrokerControlProtocolError::InvalidLength);
    }
    Ok(())
}

fn read_broker_be_u16(bytes: &[u8]) -> u16 {
    let mut value = [0_u8; 2];
    value.copy_from_slice(bytes);
    u16::from_be_bytes(value)
}

fn read_broker_be_u32(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);
    u32::from_be_bytes(value)
}

fn read_broker_be_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_be_bytes(value)
}

/// Exact identity bound into one broker Spawn WAL and its local head anchor.
///
/// One WAL exists per durable Spawn effect. The binding digest commits to the
/// complete canonical Genesis reservation, including payload digest, geometry,
/// mux/guardian builds, checkpoint, boundary, and upload identities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerSpawnWalIdentityV1 {
    journal_id: Uuid,
    broker_lineage_id: Uuid,
    mux_incarnation: Uuid,
    durable_pane_id: Uuid,
    spawn_effect_id: Uuid,
    origin_request_id: Uuid,
    spawn_payload_bytes: u64,
    spawn_payload_digest: [u8; 32],
    binding_digest: [u8; 32],
}

impl BrokerSpawnWalIdentityV1 {
    fn from_binding(
        journal_id: Uuid,
        broker_lineage_id: Uuid,
        binding: BrokerGenesisBinding,
    ) -> Result<Self, BrokerSpawnWalError> {
        binding
            .validate()
            .map_err(|_| BrokerSpawnWalError::InvalidIdentity)?;
        let identity = Self {
            journal_id,
            broker_lineage_id,
            mux_incarnation: binding.mux_incarnation,
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            origin_request_id: binding.origin_request_id,
            spawn_payload_bytes: binding.spawn_payload_bytes,
            spawn_payload_digest: binding.spawn_payload_digest,
            binding_digest: broker_genesis_binding_digest(binding),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(self) -> Result<(), BrokerSpawnWalError> {
        if self.journal_id.is_nil()
            || self.broker_lineage_id.is_nil()
            || self.mux_incarnation.is_nil()
            || self.durable_pane_id.is_nil()
            || self.spawn_effect_id.is_nil()
            || self.origin_request_id.is_nil()
            || self.spawn_payload_bytes == 0
            || self.spawn_payload_digest == [0; 32]
            || self.binding_digest == [0; 32]
        {
            return Err(BrokerSpawnWalError::InvalidIdentity);
        }
        Ok(())
    }

    #[must_use]
    pub const fn journal_id(self) -> Uuid {
        self.journal_id
    }

    #[must_use]
    pub const fn broker_lineage_id(self) -> Uuid {
        self.broker_lineage_id
    }

    #[must_use]
    pub const fn durable_pane_id(self) -> Uuid {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn spawn_effect_id(self) -> Uuid {
        self.spawn_effect_id
    }

    #[must_use]
    pub const fn origin_request_id(self) -> Uuid {
        self.origin_request_id
    }

    #[must_use]
    pub const fn spawn_payload_bytes(self) -> u64 {
        self.spawn_payload_bytes
    }

    #[must_use]
    pub const fn spawn_payload_digest(self) -> [u8; 32] {
        self.spawn_payload_digest
    }

    #[must_use]
    pub const fn binding_digest(self) -> [u8; 32] {
        self.binding_digest
    }

    /// Project the exact authenticated Spawn fields needed to fence the
    /// legacy callback in a freshly reconstructed guardian protocol state.
    pub fn durable_protocol_fence(
        self,
    ) -> Result<GuardianDurableSpawnFenceV1, BrokerSpawnWalError> {
        GuardianDurableSpawnFenceV1::new(
            self.mux_incarnation,
            self.spawn_effect_id,
            self.durable_pane_id,
            self.origin_request_id,
            self.spawn_payload_bytes,
            self.spawn_payload_digest,
        )
        .map_err(|_| BrokerSpawnWalError::InvalidIdentity)
    }
}

/// Durable phase of one broker-managed Spawn effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSpawnWalPhaseV1 {
    Intent,
    Attempted,
    SpawnObserved,
    ReplyAcknowledged,
}

impl BrokerSpawnWalPhaseV1 {
    const fn to_wire(self) -> u8 {
        match self {
            Self::Intent => 1,
            Self::Attempted => 2,
            Self::SpawnObserved => 3,
            Self::ReplyAcknowledged => 4,
        }
    }

    fn from_wire(value: u8) -> Result<Self, BrokerSpawnWalError> {
        match value {
            1 => Ok(Self::Intent),
            2 => Ok(Self::Attempted),
            3 => Ok(Self::SpawnObserved),
            4 => Ok(Self::ReplyAcknowledged),
            observed => Err(BrokerSpawnWalError::InvalidPhase { observed }),
        }
    }
}

/// Non-recycled child identity supplied by a platform identity verifier.
///
/// A PID alone is insufficient because it may be reused. The digest must bind
/// the OS process birth identity (for example pidfd/start-time provenance) to
/// the exact child returned by the one authorized spawn callback. No
/// production constructor exists in this module until that platform verifier
/// is wired; tests exercise the WAL independently of that future OS seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerKernelChildIdentityV1 {
    process_id: u32,
    broker_child_nonce: Uuid,
    kernel_start_identity_digest: [u8; 32],
}

impl BrokerKernelChildIdentityV1 {
    fn validate(self) -> Result<(), BrokerSpawnWalError> {
        if self.process_id == 0
            || self.broker_child_nonce.is_nil()
            || self.kernel_start_identity_digest == [0; 32]
        {
            return Err(BrokerSpawnWalError::InvalidChildIdentity);
        }
        Ok(())
    }

    #[must_use]
    pub const fn process_id(self) -> u32 {
        self.process_id
    }

    #[must_use]
    pub const fn broker_child_nonce(self) -> Uuid {
        self.broker_child_nonce
    }

    #[must_use]
    pub const fn kernel_start_identity_digest(self) -> [u8; 32] {
        self.kernel_start_identity_digest
    }
}

/// Live kernel authority proving that a PID still names the exact spawned
/// child incarnation whose durable digest is recorded in the Spawn WAL.
///
/// Linux retains a pidfd for the lifetime of this value. Other Unix targets
/// deliberately return [`BrokerChildIncarnationError::Unsupported`]; a PID or
/// a seconds-resolution process timestamp is not accepted as a substitute.
pub(crate) struct BrokerVerifiedKernelChildV1 {
    identity: BrokerKernelChildIdentityV1,
    #[cfg(target_os = "linux")]
    _pidfd: rustix::fd::OwnedFd,
}

impl std::fmt::Debug for BrokerVerifiedKernelChildV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerVerifiedKernelChildV1")
            .field("process_id", &self.identity.process_id)
            .field("broker_child_nonce", &"[REDACTED]")
            .field("kernel_start_identity_digest", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl BrokerVerifiedKernelChildV1 {
    #[must_use]
    pub(crate) const fn identity(&self) -> BrokerKernelChildIdentityV1 {
        self.identity
    }

    /// Verify one live child without exposing a raw descriptor or PID-only
    /// authority. The caller must retain both this value and the child handle
    /// until the Spawn-observed WAL record is synchronized.
    #[cfg(target_os = "linux")]
    pub(crate) fn verify_spawned_child(
        child: &mut (dyn Child + Send + Sync),
        broker_child_nonce: Uuid,
    ) -> Result<Self, BrokerChildIncarnationError> {
        if broker_child_nonce.is_nil() {
            return Err(BrokerChildIncarnationError::InvalidIdentity);
        }
        if child
            .try_wait()
            .map_err(|source| BrokerChildIncarnationError::Io {
                site: "pre-pidfd-child-status",
                source,
            })?
            .is_some()
        {
            return Err(BrokerChildIncarnationError::ChildNotRunning);
        }
        let process_id = child
            .process_id()
            .ok_or(BrokerChildIncarnationError::InvalidIdentity)?;
        let raw_pid =
            i32::try_from(process_id).map_err(|_| BrokerChildIncarnationError::InvalidIdentity)?;
        let pid = rustix::process::Pid::from_raw(raw_pid)
            .ok_or(BrokerChildIncarnationError::InvalidIdentity)?;
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(|error| BrokerChildIncarnationError::Io {
                site: "pidfd-open",
                source: std::io::Error::from(error),
            })?;
        let (boot_id, start_ticks, state) = linux_process_birth_identity(process_id)?;
        if matches!(state, b'Z' | b'X' | b'x') {
            return Err(BrokerChildIncarnationError::ChildNotRunning);
        }
        if child
            .try_wait()
            .map_err(|source| BrokerChildIncarnationError::Io {
                site: "post-pidfd-child-status",
                source,
            })?
            .is_some()
        {
            return Err(BrokerChildIncarnationError::ChildNotRunning);
        }
        let mut hasher = Sha256::new();
        hasher.update(b"frankenterm.guardian-broker.linux-child-incarnation.v1\0");
        hasher.update(boot_id.as_bytes());
        hasher.update(process_id.to_le_bytes());
        hasher.update(start_ticks.to_le_bytes());
        let identity = BrokerKernelChildIdentityV1 {
            process_id,
            broker_child_nonce,
            kernel_start_identity_digest: hasher.finalize().into(),
        };
        identity
            .validate()
            .map_err(|_| BrokerChildIncarnationError::InvalidIdentity)?;
        Ok(Self {
            identity,
            _pidfd: pidfd,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn verify_spawned_child(
        _child: &mut (dyn Child + Send + Sync),
        _broker_child_nonce: Uuid,
    ) -> Result<Self, BrokerChildIncarnationError> {
        Err(BrokerChildIncarnationError::Unsupported)
    }
}

#[derive(Debug, Error)]
pub enum BrokerChildIncarnationError {
    #[error("broker child incarnation identity is invalid")]
    InvalidIdentity,
    #[error("broker child exited before its kernel incarnation was verified")]
    ChildNotRunning,
    #[error("broker child incarnation verification is unsupported on this Unix target")]
    Unsupported,
    #[error("broker child incarnation probe failed at {site}")]
    Io {
        site: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("broker child incarnation procfs record is invalid")]
    InvalidProcfsRecord,
}

#[cfg(target_os = "linux")]
fn linux_process_birth_identity(
    process_id: u32,
) -> Result<(Uuid, u64, u8), BrokerChildIncarnationError> {
    let proc_directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/proc")
        .map_err(|source| BrokerChildIncarnationError::Io {
            site: "procfs-open",
            source,
        })?;
    let filesystem =
        rustix::fs::fstatfs(&proc_directory).map_err(|error| BrokerChildIncarnationError::Io {
            site: "procfs-statfs",
            source: std::io::Error::from(error),
        })?;
    if filesystem.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(BrokerChildIncarnationError::InvalidProcfsRecord);
    }
    let pid_name = process_id.to_string();
    let pid_directory = rustix::fs::openat(
        &proc_directory,
        pid_name.as_str(),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| BrokerChildIncarnationError::Io {
        site: "procfs-pid-open",
        source: std::io::Error::from(error),
    })?;
    let stat_file = rustix::fs::openat(
        &pid_directory,
        "stat",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| BrokerChildIncarnationError::Io {
        site: "procfs-stat-open",
        source: std::io::Error::from(error),
    })?;
    let stat = read_bounded_linux_proc_file(stat_file, 4096, "procfs-stat-read")?;
    let (state, start_ticks) = parse_linux_proc_stat(&stat)?;

    let boot_id_file = rustix::fs::openat(
        &proc_directory,
        "sys/kernel/random/boot_id",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| BrokerChildIncarnationError::Io {
        site: "procfs-boot-id-open",
        source: std::io::Error::from(error),
    })?;
    let boot_id = read_bounded_linux_proc_file(boot_id_file, 64, "procfs-boot-id-read")?;
    let boot_id = std::str::from_utf8(trim_ascii_whitespace(&boot_id))
        .ok()
        .and_then(|text| Uuid::parse_str(text).ok())
        .filter(|identity| !identity.is_nil())
        .ok_or(BrokerChildIncarnationError::InvalidProcfsRecord)?;
    Ok((boot_id, start_ticks, state))
}

#[cfg(target_os = "linux")]
fn read_bounded_linux_proc_file(
    file: File,
    maximum_bytes: usize,
    site: &'static str,
) -> Result<Vec<u8>, BrokerChildIncarnationError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(maximum_bytes)
        .map_err(|_| BrokerChildIncarnationError::InvalidProcfsRecord)?;
    file.take(
        u64::try_from(maximum_bytes)
            .map_err(|_| BrokerChildIncarnationError::InvalidProcfsRecord)?
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|source| BrokerChildIncarnationError::Io { site, source })?;
    if bytes.is_empty() || bytes.len() > maximum_bytes {
        return Err(BrokerChildIncarnationError::InvalidProcfsRecord);
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn parse_linux_proc_stat(bytes: &[u8]) -> Result<(u8, u64), BrokerChildIncarnationError> {
    let closing_parenthesis = bytes
        .windows(2)
        .rposition(|window| window == b") ")
        .ok_or(BrokerChildIncarnationError::InvalidProcfsRecord)?;
    let mut fields = bytes[closing_parenthesis + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let state = fields
        .next()
        .filter(|field| field.len() == 1)
        .map(|field| field[0])
        .ok_or(BrokerChildIncarnationError::InvalidProcfsRecord)?;
    let start_ticks = fields
        .nth(18)
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse::<u64>().ok())
        .filter(|ticks| *ticks != 0)
        .ok_or(BrokerChildIncarnationError::InvalidProcfsRecord)?;
    Ok((state, start_ticks))
}

#[cfg(target_os = "linux")]
fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSpawnWalTailV1 {
    Clean,
    Incomplete {
        wal_trailing_bytes: u64,
        head_trailing_bytes: u64,
    },
}

/// Content-free, authenticated Query result for one broker Spawn effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerSpawnWalStatusV1 {
    pub identity: BrokerSpawnWalIdentityV1,
    pub phase: Option<BrokerSpawnWalPhaseV1>,
    pub attempt_id: Option<Uuid>,
    pub child_identity: Option<BrokerKernelChildIdentityV1>,
    pub reply_ack_id: Option<Uuid>,
    pub committed_records: u64,
    pub committed_wal_bytes: u64,
    pub committed_head_bytes: u64,
    pub tail: BrokerSpawnWalTailV1,
    pub append_authority_withheld: bool,
    pub head_reconciliation_required: bool,
}

impl BrokerSpawnWalStatusV1 {
    /// Any durable phase is a global fence against the legacy Spawn path.
    #[must_use]
    pub const fn fences_legacy_spawn(self) -> bool {
        self.phase.is_some()
    }

    /// A synchronized attempt permanently consumes retry authority even when
    /// no later child-observation record is available.
    #[must_use]
    pub const fn spawn_outcome_is_indeterminate(self) -> bool {
        matches!(self.phase, Some(BrokerSpawnWalPhaseV1::Attempted))
    }

    /// Return the durable global Spawn fence only after at least one lifecycle
    /// record exists. Authenticated file headers alone are setup state and must
    /// not suppress an otherwise unattempted Spawn.
    pub fn durable_protocol_fence(
        self,
    ) -> Result<Option<GuardianDurableSpawnFenceV1>, BrokerSpawnWalError> {
        self.phase
            .map(|_| self.identity.durable_protocol_fence())
            .transpose()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct BrokerSpawnWalReceiptV1 {
    sequence: u64,
    phase: BrokerSpawnWalPhaseV1,
    committed_wal_bytes: u64,
    committed_head_bytes: u64,
    record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    head_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
}

impl BrokerSpawnWalReceiptV1 {
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn phase(self) -> BrokerSpawnWalPhaseV1 {
        self.phase
    }
}

impl std::fmt::Debug for BrokerSpawnWalReceiptV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerSpawnWalReceiptV1")
            .field("sequence", &self.sequence)
            .field("phase", &self.phase)
            .field("committed_wal_bytes", &self.committed_wal_bytes)
            .field("committed_head_bytes", &self.committed_head_bytes)
            .field("record_mac", &"[REDACTED]")
            .field("head_mac", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrokerSpawnWalRecordState {
    phase: BrokerSpawnWalPhaseV1,
    operation_id: Uuid,
    attempt_id: Uuid,
    child_identity: Option<BrokerKernelChildIdentityV1>,
    receipt: BrokerSpawnWalReceiptV1,
}

struct BrokerSpawnWalScan {
    committed_bytes: u64,
    trailing_bytes: u64,
    records: Vec<BrokerSpawnWalRecordState>,
}

struct BrokerSpawnHeadScan {
    committed_bytes: u64,
    trailing_bytes: u64,
    record_macs: Vec<[u8; BROKER_SPAWN_WAL_MAC_BYTES]>,
    terminal_head_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
}

/// Exclusive append/recovery authority for one broker-managed Spawn effect.
///
/// Every phase first synchronizes the WAL record and then synchronizes a
/// matching append-only local head anchor. Any write or sync error poisons the
/// live authority. Recovery accepts a clean exact pair or one complete WAL
/// record ahead of the head (the only valid crash cut), but never grants append
/// authority until the service revalidates both descriptors and reconciles the
/// head. A valid-prefix rollback by a hostile same-UID actor is outside this
/// local crash-durability threat model.
pub struct BrokerSpawnJournalV1 {
    wal: File,
    head: File,
    identity: BrokerSpawnWalIdentityV1,
    authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    head_header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    committed_wal_bytes: u64,
    committed_head_bytes: u64,
    wal_trailing_bytes: u64,
    head_trailing_bytes: u64,
    records: Vec<BrokerSpawnWalRecordState>,
    terminal_head_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    directory_entry_sync_required: bool,
    recovery_append_authority_withheld: bool,
    head_reconciliation_required: bool,
    poisoned: bool,
    #[cfg(test)]
    injected_fault: Option<BrokerSpawnWalInjectedFault>,
}

/// Pinned, exclusive authority over the bounded broker Spawn-WAL directory.
///
/// The directory must already exist as an absolute, normalized, current-user,
/// owner-only directory. Every name is enumerated through the pinned directory
/// descriptor; unknown names and incomplete WAL/head pairs fail closed and are
/// never removed. The retained lock descriptor prevents two broker processes
/// from reconciling or appending the same catalog concurrently.
pub(crate) struct BrokerSpawnWalCatalogV1 {
    directory_path: PathBuf,
    directory: File,
    lock: File,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct BrokerSpawnWalCatalogPairV1 {
    wal_name: Option<OsString>,
    head_name: Option<OsString>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrokerSpawnWalFileIdentityV1 {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
    bytes: u64,
}

/// Opaque proof that the service revalidated the pinned WAL, head, key, and
/// parent-directory identities after recovery.
pub(crate) struct BrokerSpawnWalFilesystemRevalidationV1 {
    identity: BrokerSpawnWalIdentityV1,
    observed_wal_bytes: u64,
    observed_head_bytes: u64,
}

/// Nonduplicable authority to invoke one Spawn callback after durable Attempt.
#[must_use = "a durable broker Spawn attempt permit must be consumed exactly once"]
pub struct BrokerSpawnAttemptPermitV1 {
    identity: BrokerSpawnWalIdentityV1,
    attempt_id: Uuid,
    attempt_record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
}

pub enum BrokerSpawnAttemptAdmissionV1 {
    Authorized(BrokerSpawnAttemptPermitV1),
    Reconciled(BrokerSpawnWalStatusV1),
}

/// Permit produced only by the successful callback invoked through a durable
/// attempt. It binds the observed child identity to that exact attempt.
#[must_use = "a successful broker Spawn observation must be durably committed"]
pub struct BrokerSpawnObservationPermitV1 {
    identity: BrokerSpawnWalIdentityV1,
    attempt_id: Uuid,
    attempt_record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    child_identity: BrokerKernelChildIdentityV1,
}

pub enum BrokerSpawnAttemptExecutionV1<T> {
    EffectSucceeded {
        value: T,
        observation: Box<BrokerSpawnObservationPermitV1>,
    },
    OutcomeIndeterminate {
        retained_value: Option<T>,
    },
}

#[derive(Debug, Error)]
pub enum BrokerSpawnWalError {
    #[error("broker Spawn WAL identity is invalid")]
    InvalidIdentity,
    #[error("broker Spawn WAL child identity is invalid")]
    InvalidChildIdentity,
    #[error("broker Spawn WAL descriptor is not a regular file")]
    NotRegularFile,
    #[error("broker Spawn WAL parent descriptor is not a directory")]
    NotDirectory,
    #[error("broker Spawn WAL catalog path is invalid")]
    InvalidCatalogPath,
    #[error("broker Spawn WAL catalog path or descriptor identity is insecure")]
    InsecureCatalogIdentity,
    #[error("broker Spawn WAL catalog contains an unknown or noncanonical entry")]
    UnexpectedCatalogEntry,
    #[error("broker Spawn WAL catalog is missing one member of a WAL/head pair")]
    IncompleteCatalogPair,
    #[error("broker Spawn WAL catalog already has an active process owner")]
    CatalogAlreadyOwned,
    #[error("broker Spawn WAL catalog lock is unsupported on this Unix target")]
    CatalogLockUnsupported,
    #[error("broker Spawn WAL catalog file name does not match its authenticated identity")]
    CatalogIdentityMismatch,
    #[error("new broker Spawn WAL or head descriptor is not empty")]
    NewJournalNotEmpty,
    #[error("broker Spawn WAL file header is torn")]
    TornFileHeader,
    #[error("broker Spawn WAL file magic is invalid")]
    InvalidFileMagic,
    #[error("unsupported broker Spawn WAL version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("broker Spawn WAL file header length is invalid")]
    InvalidFileHeaderLength,
    #[error("broker Spawn WAL file header is noncanonical")]
    NonCanonicalFileHeader,
    #[error("broker Spawn WAL key identity does not match")]
    KeyIdentityMismatch,
    #[error("broker Spawn WAL identity does not match")]
    IdentityMismatch,
    #[error("broker Spawn WAL authentication failed")]
    AuthenticationFailed,
    #[error("broker Spawn WAL record magic or length is invalid")]
    InvalidRecordFraming,
    #[error("broker Spawn WAL record is noncanonical")]
    NonCanonicalRecord,
    #[error("broker Spawn WAL phase value {observed} is invalid")]
    InvalidPhase { observed: u8 },
    #[error("broker Spawn WAL sequence mismatch")]
    SequenceMismatch,
    #[error("broker Spawn WAL phase transition is invalid")]
    InvalidTransition,
    #[error("broker Spawn WAL record chain does not match")]
    RecordChainMismatch,
    #[error("broker Spawn WAL local head is ahead of or conflicts with the WAL")]
    HeadAnchorMismatch,
    #[error("broker Spawn WAL has more than one unreconciled head record")]
    HeadReconciliationGap,
    #[error("broker Spawn WAL has an incomplete tail and is sealed")]
    IncompleteTail,
    #[error("broker Spawn WAL must synchronize both new directory entries before append")]
    DirectoryEntryNotDurable,
    #[error("broker Spawn WAL recovery append authority is withheld")]
    RecoveryAuthorityUnavailable,
    #[error("broker Spawn WAL filesystem revalidation authority does not match")]
    FilesystemRevalidationMismatch,
    #[error("broker Spawn WAL is poisoned after an ambiguous append or sync failure")]
    Poisoned,
    #[error("broker Spawn WAL length changed outside its exclusive owner")]
    ExternalLengthChange,
    #[error("broker Spawn WAL effect identity conflicts with durable state")]
    EffectIdentityConflict,
    #[error("broker Spawn WAL capacity is exhausted")]
    CapacityExhausted,
    #[error("broker Spawn WAL I/O failed")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrokerSpawnWalInjectedFault {
    BeforeWalWrite,
    AfterWalSyncBeforeHead,
    BeforeHeadSync,
}

fn broker_genesis_binding_digest(binding: BrokerGenesisBinding) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"frankenterm.guardian-broker.genesis-binding.v1\0");
    hasher.update(binding.mux_incarnation.as_bytes());
    hasher.update(binding.spawn_effect_id.as_bytes());
    hasher.update(binding.durable_pane_id.as_bytes());
    hasher.update(binding.origin_request_id.as_bytes());
    hasher.update(binding.spawn_payload_bytes.to_le_bytes());
    hasher.update(binding.spawn_payload_digest);
    hasher.update(binding.spawning_mux_build_identity_digest);
    hasher.update(binding.live_guardian_build_identity_digest);
    hasher.update(binding.rows.to_le_bytes());
    hasher.update(binding.cols.to_le_bytes());
    hasher.update(binding.pixel_width.to_le_bytes());
    hasher.update(binding.pixel_height.to_le_bytes());
    hasher.update(binding.checkpoint_identity_digest);
    hasher.update(binding.boundary_identity_digest);
    hasher.update(binding.upload_id.as_bytes());
    hasher.finalize().into()
}

fn broker_spawn_wal_authenticate(
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    bytes: &[u8],
) -> Result<[u8; BROKER_SPAWN_WAL_MAC_BYTES], BrokerSpawnWalError> {
    authenticator
        .authenticate(bytes)
        .map_err(|_| BrokerSpawnWalError::AuthenticationFailed)
}

fn broker_spawn_wal_verify(
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    bytes: &[u8],
    tag: &[u8],
) -> Result<(), BrokerSpawnWalError> {
    authenticator
        .verify(bytes, tag)
        .map_err(|_| BrokerSpawnWalError::AuthenticationFailed)
}

fn encode_broker_spawn_file_header(
    magic: [u8; 8],
    identity: BrokerSpawnWalIdentityV1,
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
) -> Result<[u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES], BrokerSpawnWalError> {
    identity.validate()?;
    let mut header = [0_u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES];
    header[0..8].copy_from_slice(&magic);
    header[8..12].copy_from_slice(&BROKER_SPAWN_WAL_FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U32.to_le_bytes());
    header[16..32].copy_from_slice(identity.journal_id.as_bytes());
    header[32..48].copy_from_slice(identity.broker_lineage_id.as_bytes());
    header[48..64].copy_from_slice(identity.mux_incarnation.as_bytes());
    header[64..80].copy_from_slice(identity.durable_pane_id.as_bytes());
    header[80..96].copy_from_slice(identity.spawn_effect_id.as_bytes());
    header[96..112].copy_from_slice(identity.origin_request_id.as_bytes());
    header[112..120].copy_from_slice(&identity.spawn_payload_bytes.to_le_bytes());
    header[120..152].copy_from_slice(&identity.spawn_payload_digest);
    header[152..160].copy_from_slice(&authenticator.key_id());
    header[160..192].copy_from_slice(&identity.binding_digest);
    let tag = broker_spawn_wal_authenticate(
        authenticator,
        &header[..BROKER_SPAWN_WAL_AUTHENTICATED_HEADER_BYTES],
    )?;
    header[192..224].copy_from_slice(&tag);
    Ok(header)
}

fn validate_broker_spawn_file_header(
    header: &[u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES],
    magic: [u8; 8],
    expected: BrokerSpawnWalIdentityV1,
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
) -> Result<[u8; BROKER_SPAWN_WAL_MAC_BYTES], BrokerSpawnWalError> {
    let (observed, tag) = decode_broker_spawn_file_header(header, magic, authenticator)?;
    if observed != expected {
        return Err(BrokerSpawnWalError::IdentityMismatch);
    }
    Ok(tag)
}

fn decode_broker_spawn_file_header(
    header: &[u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES],
    magic: [u8; 8],
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
) -> Result<(BrokerSpawnWalIdentityV1, [u8; BROKER_SPAWN_WAL_MAC_BYTES]), BrokerSpawnWalError> {
    if header[0..8] != magic {
        return Err(BrokerSpawnWalError::InvalidFileMagic);
    }
    let version = read_broker_u32(&header[8..12]);
    if version != BROKER_SPAWN_WAL_FORMAT_VERSION {
        return Err(BrokerSpawnWalError::UnsupportedVersion { observed: version });
    }
    if read_broker_u32(&header[12..16]) != BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U32 {
        return Err(BrokerSpawnWalError::InvalidFileHeaderLength);
    }
    if header[152..160] != authenticator.key_id() {
        return Err(BrokerSpawnWalError::KeyIdentityMismatch);
    }
    let observed = BrokerSpawnWalIdentityV1 {
        journal_id: read_broker_uuid(&header[16..32]),
        broker_lineage_id: read_broker_uuid(&header[32..48]),
        mux_incarnation: read_broker_uuid(&header[48..64]),
        durable_pane_id: read_broker_uuid(&header[64..80]),
        spawn_effect_id: read_broker_uuid(&header[80..96]),
        origin_request_id: read_broker_uuid(&header[96..112]),
        spawn_payload_bytes: read_broker_u64(&header[112..120]),
        spawn_payload_digest: read_broker_array_32(&header[120..152]),
        binding_digest: read_broker_array_32(&header[160..192]),
    };
    observed.validate()?;
    broker_spawn_wal_verify(
        authenticator,
        &header[..BROKER_SPAWN_WAL_AUTHENTICATED_HEADER_BYTES],
        &header[192..224],
    )?;
    Ok((observed, read_broker_array_32(&header[192..224])))
}

fn encode_broker_spawn_wal_record(
    header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    sequence: u64,
    phase: BrokerSpawnWalPhaseV1,
    operation_id: Uuid,
    attempt_id: Uuid,
    child_identity: Option<BrokerKernelChildIdentityV1>,
    previous_record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
) -> Result<[u8; BROKER_SPAWN_WAL_RECORD_BYTES], BrokerSpawnWalError> {
    validate_broker_spawn_record_fields(phase, operation_id, attempt_id, child_identity)?;
    let mut record = [0_u8; BROKER_SPAWN_WAL_RECORD_BYTES];
    record[0..8].copy_from_slice(&BROKER_SPAWN_WAL_RECORD_MAGIC);
    record[8..12].copy_from_slice(&BROKER_SPAWN_WAL_RECORD_BYTES_U32.to_le_bytes());
    record[12] = phase.to_wire();
    record[16..24].copy_from_slice(&sequence.to_le_bytes());
    record[24..40].copy_from_slice(operation_id.as_bytes());
    record[40..56].copy_from_slice(attempt_id.as_bytes());
    if let Some(child_identity) = child_identity {
        record[56..72].copy_from_slice(child_identity.broker_child_nonce.as_bytes());
        record[72..76].copy_from_slice(&child_identity.process_id.to_le_bytes());
        record[80..112].copy_from_slice(&child_identity.kernel_start_identity_digest);
    }
    record[112..144].copy_from_slice(&previous_record_mac);
    let mut authenticated =
        [0_u8; BROKER_SPAWN_WAL_MAC_BYTES + BROKER_SPAWN_WAL_AUTHENTICATED_RECORD_BYTES];
    authenticated[..BROKER_SPAWN_WAL_MAC_BYTES].copy_from_slice(&header_mac);
    authenticated[BROKER_SPAWN_WAL_MAC_BYTES..]
        .copy_from_slice(&record[..BROKER_SPAWN_WAL_AUTHENTICATED_RECORD_BYTES]);
    let tag = broker_spawn_wal_authenticate(authenticator, &authenticated)?;
    record[144..176].copy_from_slice(&tag);
    Ok(record)
}

#[allow(clippy::too_many_arguments)]
fn decode_broker_spawn_wal_record(
    record: &[u8; BROKER_SPAWN_WAL_RECORD_BYTES],
    header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    expected_sequence: u64,
    expected_previous_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    previous: Option<BrokerSpawnWalRecordState>,
) -> Result<BrokerSpawnWalRecordState, BrokerSpawnWalError> {
    if record[0..8] != BROKER_SPAWN_WAL_RECORD_MAGIC
        || read_broker_u32(&record[8..12]) != BROKER_SPAWN_WAL_RECORD_BYTES_U32
    {
        return Err(BrokerSpawnWalError::InvalidRecordFraming);
    }
    if record[13..16].iter().any(|byte| *byte != 0) || record[76..80].iter().any(|byte| *byte != 0)
    {
        return Err(BrokerSpawnWalError::NonCanonicalRecord);
    }
    let sequence = read_broker_u64(&record[16..24]);
    if sequence != expected_sequence {
        return Err(BrokerSpawnWalError::SequenceMismatch);
    }
    if record[112..144] != expected_previous_mac {
        return Err(BrokerSpawnWalError::RecordChainMismatch);
    }
    let mut authenticated =
        [0_u8; BROKER_SPAWN_WAL_MAC_BYTES + BROKER_SPAWN_WAL_AUTHENTICATED_RECORD_BYTES];
    authenticated[..BROKER_SPAWN_WAL_MAC_BYTES].copy_from_slice(&header_mac);
    authenticated[BROKER_SPAWN_WAL_MAC_BYTES..]
        .copy_from_slice(&record[..BROKER_SPAWN_WAL_AUTHENTICATED_RECORD_BYTES]);
    broker_spawn_wal_verify(authenticator, &authenticated, &record[144..176])?;

    let phase = BrokerSpawnWalPhaseV1::from_wire(record[12])?;
    let operation_id = read_broker_uuid(&record[24..40]);
    let attempt_id = read_broker_uuid(&record[40..56]);
    let process_id = read_broker_u32(&record[72..76]);
    let broker_child_nonce = read_broker_uuid(&record[56..72]);
    let kernel_start_identity_digest = read_broker_array_32(&record[80..112]);
    let child_identity = if process_id == 0
        && broker_child_nonce.is_nil()
        && kernel_start_identity_digest == [0; 32]
    {
        None
    } else {
        Some(BrokerKernelChildIdentityV1 {
            process_id,
            broker_child_nonce,
            kernel_start_identity_digest,
        })
    };
    validate_broker_spawn_record_fields(phase, operation_id, attempt_id, child_identity)?;
    validate_broker_spawn_record_transition(previous, phase, attempt_id, child_identity)?;
    let record_mac = read_broker_array_32(&record[144..176]);
    Ok(BrokerSpawnWalRecordState {
        phase,
        operation_id,
        attempt_id,
        child_identity,
        receipt: BrokerSpawnWalReceiptV1 {
            sequence,
            phase,
            committed_wal_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
                + (sequence + 1) * BROKER_SPAWN_WAL_RECORD_BYTES_U64,
            committed_head_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
            record_mac,
            head_mac: [0; BROKER_SPAWN_WAL_MAC_BYTES],
        },
    })
}

fn encode_broker_spawn_head_record(
    head_header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    sequence: u64,
    wal_record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    previous_head_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
) -> Result<[u8; BROKER_SPAWN_HEAD_RECORD_BYTES], BrokerSpawnWalError> {
    let mut record = [0_u8; BROKER_SPAWN_HEAD_RECORD_BYTES];
    record[0..8].copy_from_slice(&BROKER_SPAWN_HEAD_RECORD_MAGIC);
    record[8..12].copy_from_slice(&BROKER_SPAWN_HEAD_RECORD_BYTES_U32.to_le_bytes());
    record[16..24].copy_from_slice(&sequence.to_le_bytes());
    record[24..56].copy_from_slice(&wal_record_mac);
    record[56..88].copy_from_slice(&previous_head_mac);
    let mut authenticated =
        [0_u8; BROKER_SPAWN_WAL_MAC_BYTES + BROKER_SPAWN_HEAD_AUTHENTICATED_RECORD_BYTES];
    authenticated[..BROKER_SPAWN_WAL_MAC_BYTES].copy_from_slice(&head_header_mac);
    authenticated[BROKER_SPAWN_WAL_MAC_BYTES..]
        .copy_from_slice(&record[..BROKER_SPAWN_HEAD_AUTHENTICATED_RECORD_BYTES]);
    let tag = broker_spawn_wal_authenticate(authenticator, &authenticated)?;
    record[88..120].copy_from_slice(&tag);
    Ok(record)
}

fn decode_broker_spawn_head_record(
    record: &[u8; BROKER_SPAWN_HEAD_RECORD_BYTES],
    head_header_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    expected_sequence: u64,
    expected_wal_record_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
    expected_previous_head_mac: [u8; BROKER_SPAWN_WAL_MAC_BYTES],
) -> Result<[u8; BROKER_SPAWN_WAL_MAC_BYTES], BrokerSpawnWalError> {
    if record[0..8] != BROKER_SPAWN_HEAD_RECORD_MAGIC
        || read_broker_u32(&record[8..12]) != BROKER_SPAWN_HEAD_RECORD_BYTES_U32
    {
        return Err(BrokerSpawnWalError::InvalidRecordFraming);
    }
    if record[12..16].iter().any(|byte| *byte != 0)
        || read_broker_u64(&record[16..24]) != expected_sequence
        || record[24..56] != expected_wal_record_mac
        || record[56..88] != expected_previous_head_mac
    {
        return Err(BrokerSpawnWalError::HeadAnchorMismatch);
    }
    let mut authenticated =
        [0_u8; BROKER_SPAWN_WAL_MAC_BYTES + BROKER_SPAWN_HEAD_AUTHENTICATED_RECORD_BYTES];
    authenticated[..BROKER_SPAWN_WAL_MAC_BYTES].copy_from_slice(&head_header_mac);
    authenticated[BROKER_SPAWN_WAL_MAC_BYTES..]
        .copy_from_slice(&record[..BROKER_SPAWN_HEAD_AUTHENTICATED_RECORD_BYTES]);
    broker_spawn_wal_verify(authenticator, &authenticated, &record[88..120])?;
    Ok(read_broker_array_32(&record[88..120]))
}

fn validate_broker_spawn_record_fields(
    phase: BrokerSpawnWalPhaseV1,
    operation_id: Uuid,
    attempt_id: Uuid,
    child_identity: Option<BrokerKernelChildIdentityV1>,
) -> Result<(), BrokerSpawnWalError> {
    if operation_id.is_nil() {
        return Err(BrokerSpawnWalError::NonCanonicalRecord);
    }
    match phase {
        BrokerSpawnWalPhaseV1::Intent if attempt_id.is_nil() && child_identity.is_none() => {}
        BrokerSpawnWalPhaseV1::Attempted
            if !attempt_id.is_nil() && operation_id == attempt_id && child_identity.is_none() => {}
        BrokerSpawnWalPhaseV1::SpawnObserved | BrokerSpawnWalPhaseV1::ReplyAcknowledged
            if !attempt_id.is_nil() && child_identity.is_some() =>
        {
            child_identity.expect("checked child identity").validate()?;
        }
        _ => return Err(BrokerSpawnWalError::NonCanonicalRecord),
    }
    Ok(())
}

fn validate_broker_spawn_record_transition(
    previous: Option<BrokerSpawnWalRecordState>,
    phase: BrokerSpawnWalPhaseV1,
    attempt_id: Uuid,
    child_identity: Option<BrokerKernelChildIdentityV1>,
) -> Result<(), BrokerSpawnWalError> {
    match (previous, phase) {
        (None, BrokerSpawnWalPhaseV1::Intent) => Ok(()),
        (Some(previous), BrokerSpawnWalPhaseV1::Attempted)
            if previous.phase == BrokerSpawnWalPhaseV1::Intent =>
        {
            Ok(())
        }
        (Some(previous), BrokerSpawnWalPhaseV1::SpawnObserved)
            if previous.phase == BrokerSpawnWalPhaseV1::Attempted
                && previous.attempt_id == attempt_id =>
        {
            Ok(())
        }
        (Some(previous), BrokerSpawnWalPhaseV1::ReplyAcknowledged)
            if previous.phase == BrokerSpawnWalPhaseV1::SpawnObserved
                && previous.attempt_id == attempt_id
                && previous.child_identity == child_identity =>
        {
            Ok(())
        }
        _ => Err(BrokerSpawnWalError::InvalidTransition),
    }
}

fn read_broker_u32(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

fn read_broker_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

fn read_broker_uuid(bytes: &[u8]) -> Uuid {
    let mut value = [0_u8; 16];
    value.copy_from_slice(bytes);
    Uuid::from_bytes(value)
}

fn read_broker_array_32(bytes: &[u8]) -> [u8; 32] {
    let mut value = [0_u8; 32];
    value.copy_from_slice(bytes);
    value
}

fn scan_broker_spawn_wal(
    wal: &mut File,
    identity: BrokerSpawnWalIdentityV1,
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
) -> Result<([u8; BROKER_SPAWN_WAL_MAC_BYTES], BrokerSpawnWalScan), BrokerSpawnWalError> {
    let metadata = wal.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(BrokerSpawnWalError::NotRegularFile);
    }
    let physical_bytes = metadata.len();
    if physical_bytes < BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64 {
        return Err(BrokerSpawnWalError::TornFileHeader);
    }
    let maximum_physical_bytes = BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
        .checked_add(
            BROKER_SPAWN_WAL_MAX_RECORDS
                .checked_mul(BROKER_SPAWN_WAL_RECORD_BYTES_U64)
                .ok_or(BrokerSpawnWalError::CapacityExhausted)?,
        )
        .and_then(|bytes| bytes.checked_add(BROKER_SPAWN_WAL_RECORD_BYTES_U64 - 1))
        .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
    if physical_bytes > maximum_physical_bytes {
        return Err(BrokerSpawnWalError::CapacityExhausted);
    }
    wal.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES];
    wal.read_exact(&mut header)?;
    let header_mac = validate_broker_spawn_file_header(
        &header,
        BROKER_SPAWN_WAL_FILE_MAGIC,
        identity,
        authenticator,
    )?;
    let record_region = physical_bytes - BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64;
    let complete_records = record_region / BROKER_SPAWN_WAL_RECORD_BYTES_U64;
    if complete_records > BROKER_SPAWN_WAL_MAX_RECORDS {
        return Err(BrokerSpawnWalError::CapacityExhausted);
    }
    let trailing_bytes = record_region % BROKER_SPAWN_WAL_RECORD_BYTES_U64;
    let capacity =
        usize::try_from(complete_records).map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(capacity)
        .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
    let mut previous_mac = header_mac;
    let mut previous = None;
    for sequence in 0..complete_records {
        let mut record = [0_u8; BROKER_SPAWN_WAL_RECORD_BYTES];
        wal.read_exact(&mut record)?;
        let decoded = decode_broker_spawn_wal_record(
            &record,
            header_mac,
            authenticator,
            sequence,
            previous_mac,
            previous,
        )?;
        previous_mac = decoded.receipt.record_mac;
        previous = Some(decoded);
        records.push(decoded);
    }
    Ok((
        header_mac,
        BrokerSpawnWalScan {
            committed_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
                + complete_records * BROKER_SPAWN_WAL_RECORD_BYTES_U64,
            trailing_bytes,
            records,
        },
    ))
}

fn scan_broker_spawn_head(
    head: &mut File,
    identity: BrokerSpawnWalIdentityV1,
    authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    wal_records: &[BrokerSpawnWalRecordState],
) -> Result<([u8; BROKER_SPAWN_WAL_MAC_BYTES], BrokerSpawnHeadScan), BrokerSpawnWalError> {
    let metadata = head.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(BrokerSpawnWalError::NotRegularFile);
    }
    let physical_bytes = metadata.len();
    if physical_bytes < BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64 {
        return Err(BrokerSpawnWalError::TornFileHeader);
    }
    let maximum_physical_bytes = BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
        .checked_add(
            BROKER_SPAWN_WAL_MAX_RECORDS
                .checked_mul(BROKER_SPAWN_HEAD_RECORD_BYTES_U64)
                .ok_or(BrokerSpawnWalError::CapacityExhausted)?,
        )
        .and_then(|bytes| bytes.checked_add(BROKER_SPAWN_HEAD_RECORD_BYTES_U64 - 1))
        .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
    if physical_bytes > maximum_physical_bytes {
        return Err(BrokerSpawnWalError::CapacityExhausted);
    }
    head.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES];
    head.read_exact(&mut header)?;
    let head_header_mac = validate_broker_spawn_file_header(
        &header,
        BROKER_SPAWN_HEAD_FILE_MAGIC,
        identity,
        authenticator,
    )?;
    let record_region = physical_bytes - BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64;
    let complete_records = record_region / BROKER_SPAWN_HEAD_RECORD_BYTES_U64;
    if complete_records > BROKER_SPAWN_WAL_MAX_RECORDS
        || complete_records
            > u64::try_from(wal_records.len())
                .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?
    {
        return Err(BrokerSpawnWalError::HeadAnchorMismatch);
    }
    let trailing_bytes = record_region % BROKER_SPAWN_HEAD_RECORD_BYTES_U64;
    let capacity =
        usize::try_from(complete_records).map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
    let mut record_macs = Vec::new();
    record_macs
        .try_reserve_exact(capacity)
        .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
    let mut previous_head_mac = head_header_mac;
    for sequence in 0..complete_records {
        let index =
            usize::try_from(sequence).map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        let wal_record = wal_records
            .get(index)
            .ok_or(BrokerSpawnWalError::HeadAnchorMismatch)?;
        let mut record = [0_u8; BROKER_SPAWN_HEAD_RECORD_BYTES];
        head.read_exact(&mut record)?;
        let head_mac = decode_broker_spawn_head_record(
            &record,
            head_header_mac,
            authenticator,
            sequence,
            wal_record.receipt.record_mac,
            previous_head_mac,
        )?;
        previous_head_mac = head_mac;
        record_macs.push(head_mac);
    }
    Ok((
        head_header_mac,
        BrokerSpawnHeadScan {
            committed_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
                + complete_records * BROKER_SPAWN_HEAD_RECORD_BYTES_U64,
            trailing_bytes,
            record_macs,
            terminal_head_mac: previous_head_mac,
        },
    ))
}

fn broker_spawn_catalog_wal_name(journal_id: Uuid) -> OsString {
    format!("{BROKER_SPAWN_CATALOG_PREFIX}{journal_id}{BROKER_SPAWN_CATALOG_WAL_SUFFIX}").into()
}

fn broker_spawn_catalog_head_name(journal_id: Uuid) -> OsString {
    format!("{BROKER_SPAWN_CATALOG_PREFIX}{journal_id}{BROKER_SPAWN_CATALOG_HEAD_SUFFIX}").into()
}

fn parse_broker_spawn_catalog_name(name: &OsStr) -> Result<(Uuid, bool), BrokerSpawnWalError> {
    let text = name
        .to_str()
        .ok_or(BrokerSpawnWalError::UnexpectedCatalogEntry)?;
    let (identifier, is_wal) = if let Some(identifier) = text
        .strip_prefix(BROKER_SPAWN_CATALOG_PREFIX)
        .and_then(|rest| rest.strip_suffix(BROKER_SPAWN_CATALOG_WAL_SUFFIX))
    {
        (identifier, true)
    } else if let Some(identifier) = text
        .strip_prefix(BROKER_SPAWN_CATALOG_PREFIX)
        .and_then(|rest| rest.strip_suffix(BROKER_SPAWN_CATALOG_HEAD_SUFFIX))
    {
        (identifier, false)
    } else {
        return Err(BrokerSpawnWalError::UnexpectedCatalogEntry);
    };
    let journal_id =
        Uuid::parse_str(identifier).map_err(|_| BrokerSpawnWalError::UnexpectedCatalogEntry)?;
    if journal_id.is_nil()
        || (is_wal && broker_spawn_catalog_wal_name(journal_id) != name)
        || (!is_wal && broker_spawn_catalog_head_name(journal_id) != name)
    {
        return Err(BrokerSpawnWalError::UnexpectedCatalogEntry);
    }
    Ok((journal_id, is_wal))
}

fn validate_broker_spawn_catalog_path(path: &Path) -> Result<(), BrokerSpawnWalError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(BrokerSpawnWalError::InvalidCatalogPath);
    }
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => current.push(part),
            _ => return Err(BrokerSpawnWalError::InvalidCatalogPath),
        }
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
        }
        if current != path && metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
            return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
        }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(())
}

fn broker_spawn_file_identity_from_metadata(metadata: &Metadata) -> BrokerSpawnWalFileIdentityV1 {
    BrokerSpawnWalFileIdentityV1 {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        links: metadata.nlink(),
        bytes: metadata.len(),
    }
}

fn validate_broker_spawn_catalog_directory_metadata(
    metadata: &Metadata,
) -> Result<(), BrokerSpawnWalError> {
    if !metadata.is_dir()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(())
}

fn require_same_broker_spawn_catalog_object(
    left: &Metadata,
    right: &Metadata,
) -> Result<(), BrokerSpawnWalError> {
    if left.dev() != right.dev()
        || left.ino() != right.ino()
        || left.mode() != right.mode()
        || left.uid() != right.uid()
        || left.nlink() != right.nlink()
    {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(())
}

fn validate_broker_spawn_catalog_file_metadata(
    metadata: &Metadata,
    minimum_bytes: u64,
    maximum_bytes: u64,
) -> Result<BrokerSpawnWalFileIdentityV1, BrokerSpawnWalError> {
    if !metadata.is_file()
        || metadata.uid() != geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() < minimum_bytes
        || metadata.len() > maximum_bytes
    {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(broker_spawn_file_identity_from_metadata(metadata))
}

fn broker_spawn_catalog_stat_identity_at(
    directory: &File,
    name: &OsStr,
    minimum_bytes: u64,
    maximum_bytes: u64,
) -> Result<BrokerSpawnWalFileIdentityV1, BrokerSpawnWalError> {
    let metadata = rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(std::io::Error::from)?;
    #[allow(clippy::useless_conversion)]
    let identity = BrokerSpawnWalFileIdentityV1 {
        device: u64::try_from(metadata.st_dev)
            .map_err(|_| BrokerSpawnWalError::InsecureCatalogIdentity)?,
        inode: u64::try_from(metadata.st_ino)
            .map_err(|_| BrokerSpawnWalError::InsecureCatalogIdentity)?,
        mode: u32::from(metadata.st_mode),
        owner: u32::try_from(metadata.st_uid)
            .map_err(|_| BrokerSpawnWalError::InsecureCatalogIdentity)?,
        links: u64::from(metadata.st_nlink),
        bytes: u64::try_from(metadata.st_size)
            .map_err(|_| BrokerSpawnWalError::InsecureCatalogIdentity)?,
    };
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::RegularFile
        || identity.owner != geteuid().as_raw()
        || identity.mode & 0o777 != 0o600
        || identity.links != 1
        || identity.bytes < minimum_bytes
        || identity.bytes > maximum_bytes
    {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(identity)
}

fn open_broker_spawn_catalog_file_at(
    directory: &File,
    name: &OsStr,
    create_new: bool,
) -> std::io::Result<File> {
    let mut flags =
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW;
    if create_new {
        flags |= rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL;
    }
    let file = rustix::fs::openat(
        directory,
        name,
        flags,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    if create_new {
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(0o600))
            .map_err(std::io::Error::from)?;
    }
    Ok(file)
}

fn open_revalidated_broker_spawn_catalog_file(
    directory: &File,
    name: &OsStr,
    minimum_bytes: u64,
    maximum_bytes: u64,
) -> Result<File, BrokerSpawnWalError> {
    let before =
        broker_spawn_catalog_stat_identity_at(directory, name, minimum_bytes, maximum_bytes)?;
    let file = open_broker_spawn_catalog_file_at(directory, name, false)?;
    let opened = validate_broker_spawn_catalog_file_metadata(
        &file.metadata()?,
        minimum_bytes,
        maximum_bytes,
    )?;
    let after =
        broker_spawn_catalog_stat_identity_at(directory, name, minimum_bytes, maximum_bytes)?;
    if before != opened || opened != after {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(file)
}

fn revalidate_open_broker_spawn_catalog_file(
    directory: &File,
    name: &OsStr,
    file: &File,
    minimum_bytes: u64,
    maximum_bytes: u64,
) -> Result<BrokerSpawnWalFileIdentityV1, BrokerSpawnWalError> {
    let named =
        broker_spawn_catalog_stat_identity_at(directory, name, minimum_bytes, maximum_bytes)?;
    let opened = validate_broker_spawn_catalog_file_metadata(
        &file.metadata()?,
        minimum_bytes,
        maximum_bytes,
    )?;
    if named != opened {
        return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
    }
    Ok(opened)
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
fn lock_broker_spawn_catalog(file: &File) -> Result<(), BrokerSpawnWalError> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == ErrorKind::WouldBlock {
            BrokerSpawnWalError::CatalogAlreadyOwned
        } else {
            BrokerSpawnWalError::Io(error)
        }
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
fn lock_broker_spawn_catalog(_file: &File) -> Result<(), BrokerSpawnWalError> {
    Err(BrokerSpawnWalError::CatalogLockUnsupported)
}

impl BrokerSpawnWalCatalogV1 {
    /// Open and exclusively pin an existing owner-only catalog directory.
    pub(crate) fn open(directory_path: PathBuf) -> Result<Self, BrokerSpawnWalError> {
        validate_broker_spawn_catalog_path(&directory_path)?;
        let before = std::fs::symlink_metadata(&directory_path)?;
        validate_broker_spawn_catalog_directory_metadata(&before)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&directory_path)?;
        let opened = directory.metadata()?;
        validate_broker_spawn_catalog_directory_metadata(&opened)?;
        require_same_broker_spawn_catalog_object(&before, &opened)?;
        let after = std::fs::symlink_metadata(&directory_path)?;
        require_same_broker_spawn_catalog_object(&opened, &after)?;

        let lock_name = OsStr::new(BROKER_SPAWN_CATALOG_LOCK_NAME);
        let (lock, created) = match open_broker_spawn_catalog_file_at(&directory, lock_name, true) {
            Ok(lock) => (lock, true),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => (
                open_revalidated_broker_spawn_catalog_file(&directory, lock_name, 0, 0)?,
                false,
            ),
            Err(error) => return Err(error.into()),
        };
        if created {
            revalidate_open_broker_spawn_catalog_file(&directory, lock_name, &lock, 0, 0)?;
            lock.sync_all()?;
            directory.sync_all()?;
        }
        lock_broker_spawn_catalog(&lock)?;
        let catalog = Self {
            directory_path,
            directory,
            lock,
        };
        catalog.validate_pinned_directory()?;
        catalog.scan_pairs()?;
        Ok(catalog)
    }

    /// Create and durably publish one new authenticated WAL/head pair.
    pub(crate) fn create_spawn_journal(
        &self,
        identity: BrokerSpawnWalIdentityV1,
        authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> Result<BrokerSpawnJournalV1, BrokerSpawnWalError> {
        identity.validate()?;
        self.validate_pinned_directory()?;
        let pairs = self.scan_pairs()?;
        if pairs.len() >= GUARDIAN_MAX_PANES || pairs.contains_key(&identity.journal_id) {
            return Err(BrokerSpawnWalError::CapacityExhausted);
        }
        let wal_name = broker_spawn_catalog_wal_name(identity.journal_id);
        let head_name = broker_spawn_catalog_head_name(identity.journal_id);
        let wal = open_broker_spawn_catalog_file_at(&self.directory, &wal_name, true)?;
        revalidate_open_broker_spawn_catalog_file(&self.directory, &wal_name, &wal, 0, 0)?;
        let head = open_broker_spawn_catalog_file_at(&self.directory, &head_name, true)?;
        revalidate_open_broker_spawn_catalog_file(&self.directory, &head_name, &head, 0, 0)?;
        let mut journal = BrokerSpawnJournalV1::create(wal, head, identity, authenticator)?;
        revalidate_open_broker_spawn_catalog_file(
            &self.directory,
            &wal_name,
            &journal.wal,
            BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
            BROKER_SPAWN_WAL_MAX_PHYSICAL_BYTES,
        )?;
        revalidate_open_broker_spawn_catalog_file(
            &self.directory,
            &head_name,
            &journal.head,
            BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
            BROKER_SPAWN_HEAD_MAX_PHYSICAL_BYTES,
        )?;
        journal.sync_parent_directory_and_activate(&self.directory)?;
        self.validate_pinned_directory()?;
        Ok(journal)
    }

    /// Authenticate, revalidate, and reconcile every complete catalog pair.
    ///
    /// Unknown names, half-pairs, replaced inodes, torn records, key rotation,
    /// and catalog drift all fail the entire startup before any caller can
    /// install a partial set of durable Spawn fences.
    pub(crate) fn recover_all(
        &self,
        authenticator: &GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> Result<Vec<BrokerSpawnJournalV1>, BrokerSpawnWalError> {
        self.validate_pinned_directory()?;
        let pairs = self.scan_pairs()?;
        let mut recovered = Vec::new();
        recovered
            .try_reserve_exact(pairs.len())
            .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        for (journal_id, pair) in &pairs {
            let wal_name = pair
                .wal_name
                .as_ref()
                .ok_or(BrokerSpawnWalError::IncompleteCatalogPair)?;
            let head_name = pair
                .head_name
                .as_ref()
                .ok_or(BrokerSpawnWalError::IncompleteCatalogPair)?;
            let mut wal = open_revalidated_broker_spawn_catalog_file(
                &self.directory,
                wal_name,
                BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
                BROKER_SPAWN_WAL_MAX_PHYSICAL_BYTES,
            )?;
            let head = open_revalidated_broker_spawn_catalog_file(
                &self.directory,
                head_name,
                BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
                BROKER_SPAWN_HEAD_MAX_PHYSICAL_BYTES,
            )?;
            let mut header = [0_u8; BROKER_SPAWN_WAL_FILE_HEADER_BYTES];
            wal.read_exact(&mut header)?;
            wal.rewind()?;
            let (identity, _) = decode_broker_spawn_file_header(
                &header,
                BROKER_SPAWN_WAL_FILE_MAGIC,
                authenticator,
            )?;
            if identity.journal_id != *journal_id {
                return Err(BrokerSpawnWalError::CatalogIdentityMismatch);
            }
            recovered.push((
                wal_name.clone(),
                head_name.clone(),
                BrokerSpawnJournalV1::open(wal, head, identity, authenticator.clone())?,
            ));
        }
        if self.scan_pairs()? != pairs {
            return Err(BrokerSpawnWalError::InsecureCatalogIdentity);
        }

        let mut journals = Vec::new();
        journals
            .try_reserve_exact(recovered.len())
            .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        for (wal_name, head_name, mut journal) in recovered {
            let wal_identity = revalidate_open_broker_spawn_catalog_file(
                &self.directory,
                &wal_name,
                &journal.wal,
                BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
                BROKER_SPAWN_WAL_MAX_PHYSICAL_BYTES,
            )?;
            let head_identity = revalidate_open_broker_spawn_catalog_file(
                &self.directory,
                &head_name,
                &journal.head,
                BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
                BROKER_SPAWN_HEAD_MAX_PHYSICAL_BYTES,
            )?;
            let authority = BrokerSpawnWalFilesystemRevalidationV1::from_revalidated_filesystem(
                journal.identity,
                wal_identity.bytes,
                head_identity.bytes,
            )?;
            journal.reconcile_recovered_head_and_activate(authority)?;
            revalidate_open_broker_spawn_catalog_file(
                &self.directory,
                &head_name,
                &journal.head,
                BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
                BROKER_SPAWN_HEAD_MAX_PHYSICAL_BYTES,
            )?;
            journals.push(journal);
        }
        self.validate_pinned_directory()?;
        Ok(journals)
    }

    fn validate_pinned_directory(&self) -> Result<(), BrokerSpawnWalError> {
        validate_broker_spawn_catalog_path(&self.directory_path)?;
        let opened = self.directory.metadata()?;
        validate_broker_spawn_catalog_directory_metadata(&opened)?;
        let named = std::fs::symlink_metadata(&self.directory_path)?;
        validate_broker_spawn_catalog_directory_metadata(&named)?;
        require_same_broker_spawn_catalog_object(&opened, &named)?;
        revalidate_open_broker_spawn_catalog_file(
            &self.directory,
            OsStr::new(BROKER_SPAWN_CATALOG_LOCK_NAME),
            &self.lock,
            0,
            0,
        )?;
        Ok(())
    }

    fn scan_pairs(
        &self,
    ) -> Result<BTreeMap<Uuid, BrokerSpawnWalCatalogPairV1>, BrokerSpawnWalError> {
        let mut directory =
            rustix::fs::Dir::read_from(&self.directory).map_err(std::io::Error::from)?;
        let mut pairs: BTreeMap<Uuid, BrokerSpawnWalCatalogPairV1> = BTreeMap::new();
        let mut observed_entries = 0_usize;
        while let Some(entry) = directory.read() {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            observed_entries = observed_entries
                .checked_add(1)
                .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
            if observed_entries > BROKER_SPAWN_CATALOG_MAX_ENTRIES {
                return Err(BrokerSpawnWalError::CapacityExhausted);
            }
            let name = OsStr::from_bytes(name.to_bytes());
            if name == OsStr::new(BROKER_SPAWN_CATALOG_LOCK_NAME) {
                continue;
            }
            let (journal_id, is_wal) = parse_broker_spawn_catalog_name(name)?;
            let pair = pairs.entry(journal_id).or_default();
            let slot = if is_wal {
                &mut pair.wal_name
            } else {
                &mut pair.head_name
            };
            if slot.replace(name.to_os_string()).is_some() {
                return Err(BrokerSpawnWalError::UnexpectedCatalogEntry);
            }
        }
        if pairs
            .values()
            .any(|pair| pair.wal_name.is_none() || pair.head_name.is_none())
        {
            return Err(BrokerSpawnWalError::IncompleteCatalogPair);
        }
        Ok(pairs)
    }
}

impl BrokerSpawnJournalV1 {
    /// Initialize two exclusively created, empty regular-file descriptors.
    ///
    /// Both authenticated headers are synchronized before this returns. No
    /// record append is allowed until the exact parent directory descriptor is
    /// synchronized, making publication of both names durable as one startup
    /// barrier.
    pub(crate) fn create(
        mut wal: File,
        mut head: File,
        identity: BrokerSpawnWalIdentityV1,
        authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> Result<Self, BrokerSpawnWalError> {
        identity.validate()?;
        if !wal.metadata()?.file_type().is_file() || !head.metadata()?.file_type().is_file() {
            return Err(BrokerSpawnWalError::NotRegularFile);
        }
        if wal.metadata()?.len() != 0 || head.metadata()?.len() != 0 {
            return Err(BrokerSpawnWalError::NewJournalNotEmpty);
        }
        let wal_header =
            encode_broker_spawn_file_header(BROKER_SPAWN_WAL_FILE_MAGIC, identity, &authenticator)?;
        let head_header = encode_broker_spawn_file_header(
            BROKER_SPAWN_HEAD_FILE_MAGIC,
            identity,
            &authenticator,
        )?;
        wal.seek(SeekFrom::Start(0))?;
        wal.write_all(&wal_header)?;
        wal.sync_all()?;
        head.seek(SeekFrom::Start(0))?;
        head.write_all(&head_header)?;
        head.sync_all()?;
        let mut records = Vec::new();
        records
            .try_reserve_exact(
                usize::try_from(BROKER_SPAWN_WAL_MAX_RECORDS)
                    .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?,
            )
            .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        Ok(Self {
            wal,
            head,
            identity,
            authenticator,
            header_mac: read_broker_array_32(&wal_header[192..224]),
            head_header_mac: read_broker_array_32(&head_header[192..224]),
            committed_wal_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
            committed_head_bytes: BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64,
            wal_trailing_bytes: 0,
            head_trailing_bytes: 0,
            records,
            terminal_head_mac: read_broker_array_32(&head_header[192..224]),
            directory_entry_sync_required: true,
            recovery_append_authority_withheld: false,
            head_reconciliation_required: false,
            poisoned: false,
            #[cfg(test)]
            injected_fault: None,
        })
    }

    /// Authenticate and reconcile an existing WAL/head pair without granting
    /// append authority.
    ///
    /// A complete WAL may be exactly one record ahead of its local head: that
    /// is the deliberate crash cut after WAL `sync_all` and before head
    /// `sync_all`. Head-ahead, divergent, multi-record-ahead, or incomplete
    /// pairs fail closed and preserve all bytes.
    pub(crate) fn open(
        mut wal: File,
        mut head: File,
        identity: BrokerSpawnWalIdentityV1,
        authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> Result<Self, BrokerSpawnWalError> {
        identity.validate()?;
        let (header_mac, mut wal_scan) = scan_broker_spawn_wal(&mut wal, identity, &authenticator)?;
        let (head_header_mac, head_scan) =
            scan_broker_spawn_head(&mut head, identity, &authenticator, &wal_scan.records)?;
        let wal_record_count = wal_scan.records.len();
        let head_record_count = head_scan.record_macs.len();
        let head_gap = wal_record_count
            .checked_sub(head_record_count)
            .ok_or(BrokerSpawnWalError::HeadAnchorMismatch)?;
        if head_gap > 1 {
            return Err(BrokerSpawnWalError::HeadReconciliationGap);
        }
        for (index, head_mac) in head_scan.record_macs.iter().copied().enumerate() {
            let record = wal_scan
                .records
                .get_mut(index)
                .ok_or(BrokerSpawnWalError::HeadAnchorMismatch)?;
            record.receipt.head_mac = head_mac;
            record.receipt.committed_head_bytes = BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
                + u64::try_from(index + 1).map_err(|_| BrokerSpawnWalError::CapacityExhausted)?
                    * BROKER_SPAWN_HEAD_RECORD_BYTES_U64;
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(
                usize::try_from(BROKER_SPAWN_WAL_MAX_RECORDS)
                    .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?,
            )
            .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        records.extend(wal_scan.records);
        Ok(Self {
            wal,
            head,
            identity,
            authenticator,
            header_mac,
            head_header_mac,
            committed_wal_bytes: wal_scan.committed_bytes,
            committed_head_bytes: head_scan.committed_bytes,
            wal_trailing_bytes: wal_scan.trailing_bytes,
            head_trailing_bytes: head_scan.trailing_bytes,
            records,
            terminal_head_mac: head_scan.terminal_head_mac,
            directory_entry_sync_required: false,
            recovery_append_authority_withheld: true,
            head_reconciliation_required: head_gap == 1,
            poisoned: false,
            #[cfg(test)]
            injected_fault: None,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> BrokerSpawnWalIdentityV1 {
        self.identity
    }

    #[must_use]
    pub fn status(&self) -> BrokerSpawnWalStatusV1 {
        let terminal = self.records.last().copied();
        let child_identity = terminal.and_then(|record| record.child_identity);
        BrokerSpawnWalStatusV1 {
            identity: self.identity,
            phase: terminal.map(|record| record.phase),
            attempt_id: terminal
                .filter(|record| !record.attempt_id.is_nil())
                .map(|record| record.attempt_id),
            child_identity,
            reply_ack_id: terminal
                .filter(|record| record.phase == BrokerSpawnWalPhaseV1::ReplyAcknowledged)
                .map(|record| record.operation_id),
            committed_records: u64::try_from(self.records.len()).unwrap_or(u64::MAX),
            committed_wal_bytes: self.committed_wal_bytes,
            committed_head_bytes: self.committed_head_bytes,
            tail: if self.wal_trailing_bytes == 0 && self.head_trailing_bytes == 0 {
                BrokerSpawnWalTailV1::Clean
            } else {
                BrokerSpawnWalTailV1::Incomplete {
                    wal_trailing_bytes: self.wal_trailing_bytes,
                    head_trailing_bytes: self.head_trailing_bytes,
                }
            },
            append_authority_withheld: self.directory_entry_sync_required
                || self.recovery_append_authority_withheld
                || self.head_reconciliation_required
                || self.poisoned
                || self.wal_trailing_bytes != 0
                || self.head_trailing_bytes != 0,
            head_reconciliation_required: self.head_reconciliation_required,
        }
    }

    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Synchronize the parent that published both newly created files.
    pub fn sync_parent_directory_and_activate(
        &mut self,
        parent_directory: &File,
    ) -> Result<(), BrokerSpawnWalError> {
        if !self.directory_entry_sync_required {
            return Ok(());
        }
        if !parent_directory.metadata()?.file_type().is_dir() {
            return Err(BrokerSpawnWalError::NotDirectory);
        }
        parent_directory.sync_all()?;
        self.directory_entry_sync_required = false;
        Ok(())
    }

    /// Reconcile the only permitted recovery cut and restore append authority.
    ///
    /// The service must mint `authority` only after revalidating the no-follow,
    /// owner-only, single-link WAL/head names, their exact open inodes, the
    /// stable token identity, and the pinned parent directory. Incomplete tails
    /// are never truncated or repaired in place.
    pub(crate) fn reconcile_recovered_head_and_activate(
        &mut self,
        authority: BrokerSpawnWalFilesystemRevalidationV1,
    ) -> Result<(), BrokerSpawnWalError> {
        self.require_healthy_for_recovery()?;
        if authority.identity != self.identity {
            return Err(BrokerSpawnWalError::FilesystemRevalidationMismatch);
        }
        let authoritative_lengths = (authority.observed_wal_bytes, authority.observed_head_bytes);
        let recovered_lengths = (self.committed_wal_bytes, self.committed_head_bytes);
        if authoritative_lengths != recovered_lengths {
            return Err(BrokerSpawnWalError::FilesystemRevalidationMismatch);
        }
        let descriptor_lengths = (self.wal.metadata()?.len(), self.head.metadata()?.len());
        if descriptor_lengths != authoritative_lengths {
            return Err(BrokerSpawnWalError::FilesystemRevalidationMismatch);
        }
        if self.head_reconciliation_required {
            let record = self
                .records
                .last()
                .copied()
                .ok_or(BrokerSpawnWalError::HeadAnchorMismatch)?;
            let expected_sequence = u64::try_from(self.records.len() - 1)
                .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
            if record.receipt.sequence != expected_sequence {
                return Err(BrokerSpawnWalError::SequenceMismatch);
            }
            let encoded = encode_broker_spawn_head_record(
                self.head_header_mac,
                &self.authenticator,
                expected_sequence,
                record.receipt.record_mac,
                self.terminal_head_mac,
            )?;
            let result = (|| -> std::io::Result<()> {
                self.head.seek(SeekFrom::Start(self.committed_head_bytes))?;
                self.head.write_all(&encoded)?;
                self.head.sync_all()
            })();
            if let Err(error) = result {
                self.poisoned = true;
                return Err(BrokerSpawnWalError::Io(error));
            }
            let head_mac = read_broker_array_32(&encoded[88..120]);
            self.committed_head_bytes = self
                .committed_head_bytes
                .checked_add(BROKER_SPAWN_HEAD_RECORD_BYTES_U64)
                .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
            self.terminal_head_mac = head_mac;
            let terminal = self
                .records
                .last_mut()
                .ok_or(BrokerSpawnWalError::HeadAnchorMismatch)?;
            terminal.receipt.head_mac = head_mac;
            terminal.receipt.committed_head_bytes = self.committed_head_bytes;
            self.head_reconciliation_required = false;
        }
        self.recovery_append_authority_withheld = false;
        Ok(())
    }

    fn require_healthy_for_recovery(&self) -> Result<(), BrokerSpawnWalError> {
        if self.poisoned {
            return Err(BrokerSpawnWalError::Poisoned);
        }
        if self.directory_entry_sync_required {
            return Err(BrokerSpawnWalError::DirectoryEntryNotDurable);
        }
        if self.wal_trailing_bytes != 0 || self.head_trailing_bytes != 0 {
            return Err(BrokerSpawnWalError::IncompleteTail);
        }
        Ok(())
    }

    fn require_append_authority(&self) -> Result<(), BrokerSpawnWalError> {
        self.require_healthy_for_recovery()?;
        if self.recovery_append_authority_withheld || self.head_reconciliation_required {
            return Err(BrokerSpawnWalError::RecoveryAuthorityUnavailable);
        }
        if self.records.len()
            >= usize::try_from(BROKER_SPAWN_WAL_MAX_RECORDS)
                .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?
        {
            return Err(BrokerSpawnWalError::CapacityExhausted);
        }
        Ok(())
    }

    #[cfg(test)]
    fn inject_fault(&mut self, fault: BrokerSpawnWalInjectedFault) {
        self.injected_fault = Some(fault);
    }

    #[cfg(test)]
    fn fail_if_injected(&mut self, fault: BrokerSpawnWalInjectedFault) -> std::io::Result<()> {
        if self.injected_fault == Some(fault) {
            self.injected_fault = None;
            Err(std::io::Error::other("injected broker Spawn WAL fault"))
        } else {
            Ok(())
        }
    }
}

impl BrokerSpawnWalFilesystemRevalidationV1 {
    /// Mint recovery authority after the service has revalidated both pinned
    /// descriptors and their durable names. Raw wire fields must never call
    /// this constructor.
    pub(crate) fn from_revalidated_filesystem(
        identity: BrokerSpawnWalIdentityV1,
        observed_wal_bytes: u64,
        observed_head_bytes: u64,
    ) -> Result<Self, BrokerSpawnWalError> {
        identity.validate()?;
        if observed_wal_bytes < BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
            || observed_head_bytes < BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64
        {
            return Err(BrokerSpawnWalError::FilesystemRevalidationMismatch);
        }
        Ok(Self {
            identity,
            observed_wal_bytes,
            observed_head_bytes,
        })
    }
}

impl BrokerSpawnJournalV1 {
    /// Synchronize the durable pre-effect intent. Exact retries return the
    /// original receipt and never advance the phase.
    pub fn append_intent_and_sync(
        &mut self,
    ) -> Result<BrokerSpawnWalReceiptV1, BrokerSpawnWalError> {
        if let Some(intent) = self.records.first() {
            if intent.phase == BrokerSpawnWalPhaseV1::Intent
                && intent.operation_id == self.identity.origin_request_id
            {
                return Ok(intent.receipt);
            }
            return Err(BrokerSpawnWalError::EffectIdentityConflict);
        }
        self.append_record_and_head(
            BrokerSpawnWalPhaseV1::Intent,
            self.identity.origin_request_id,
            Uuid::nil(),
            None,
        )
    }

    /// Synchronize the one-way Attempt fence before invoking Spawn.
    ///
    /// Only a newly synchronized Attempt yields the non-cloneable callback
    /// permit. An exact retry after reply loss returns Query state and cannot
    /// invoke Spawn again.
    pub fn begin_spawn_attempt_and_sync(
        &mut self,
        attempt_id: Uuid,
    ) -> Result<BrokerSpawnAttemptAdmissionV1, BrokerSpawnWalError> {
        if attempt_id.is_nil() {
            return Err(BrokerSpawnWalError::InvalidIdentity);
        }
        let terminal = self.records.last().copied();
        match terminal {
            Some(record) if record.phase == BrokerSpawnWalPhaseV1::Intent => {
                let receipt = self.append_record_and_head(
                    BrokerSpawnWalPhaseV1::Attempted,
                    attempt_id,
                    attempt_id,
                    None,
                )?;
                Ok(BrokerSpawnAttemptAdmissionV1::Authorized(
                    BrokerSpawnAttemptPermitV1 {
                        identity: self.identity,
                        attempt_id,
                        attempt_record_mac: receipt.record_mac,
                    },
                ))
            }
            Some(record)
                if matches!(
                    record.phase,
                    BrokerSpawnWalPhaseV1::Attempted
                        | BrokerSpawnWalPhaseV1::SpawnObserved
                        | BrokerSpawnWalPhaseV1::ReplyAcknowledged
                ) && record.attempt_id == attempt_id =>
            {
                Ok(BrokerSpawnAttemptAdmissionV1::Reconciled(self.status()))
            }
            Some(_) => Err(BrokerSpawnWalError::EffectIdentityConflict),
            None => Err(BrokerSpawnWalError::InvalidTransition),
        }
    }

    /// Synchronize the exact non-recycled child identity after the authorized
    /// callback returned. Failure after the callback leaves Attempt durable and
    /// permanently ambiguous; the caller still owns the callback's returned
    /// value and must retain/quarantine it rather than retrying Spawn.
    pub fn append_spawn_observed_and_sync(
        &mut self,
        observation: BrokerSpawnObservationPermitV1,
    ) -> Result<BrokerSpawnWalReceiptV1, BrokerSpawnWalError> {
        observation.child_identity.validate()?;
        if observation.identity != self.identity {
            return Err(BrokerSpawnWalError::EffectIdentityConflict);
        }
        let attempt = self
            .records
            .get(1)
            .copied()
            .ok_or(BrokerSpawnWalError::InvalidTransition)?;
        if attempt.phase != BrokerSpawnWalPhaseV1::Attempted
            || attempt.attempt_id != observation.attempt_id
            || attempt.receipt.record_mac != observation.attempt_record_mac
        {
            return Err(BrokerSpawnWalError::EffectIdentityConflict);
        }
        match self.records.last().copied() {
            Some(record) if record.phase == BrokerSpawnWalPhaseV1::Attempted => self
                .append_record_and_head(
                    BrokerSpawnWalPhaseV1::SpawnObserved,
                    observation.attempt_id,
                    observation.attempt_id,
                    Some(observation.child_identity),
                ),
            Some(record)
                if matches!(
                    record.phase,
                    BrokerSpawnWalPhaseV1::SpawnObserved | BrokerSpawnWalPhaseV1::ReplyAcknowledged
                ) && record.attempt_id == observation.attempt_id
                    && record.child_identity == Some(observation.child_identity) =>
            {
                Ok(record.receipt)
            }
            Some(_) => Err(BrokerSpawnWalError::EffectIdentityConflict),
            None => Err(BrokerSpawnWalError::InvalidTransition),
        }
    }

    /// Durably acknowledge delivery of the exact Spawned Query result.
    ///
    /// A lost acknowledgement reply is idempotent for the same `ack_id`; no
    /// additional phase records or child effects are created.
    pub fn acknowledge_spawn_reply_and_sync(
        &mut self,
        ack_id: Uuid,
        child_identity: BrokerKernelChildIdentityV1,
    ) -> Result<BrokerSpawnWalReceiptV1, BrokerSpawnWalError> {
        if ack_id.is_nil() {
            return Err(BrokerSpawnWalError::InvalidIdentity);
        }
        child_identity.validate()?;
        match self.records.last().copied() {
            Some(record)
                if record.phase == BrokerSpawnWalPhaseV1::SpawnObserved
                    && record.child_identity == Some(child_identity) =>
            {
                self.append_record_and_head(
                    BrokerSpawnWalPhaseV1::ReplyAcknowledged,
                    ack_id,
                    record.attempt_id,
                    Some(child_identity),
                )
            }
            Some(record)
                if record.phase == BrokerSpawnWalPhaseV1::ReplyAcknowledged
                    && record.operation_id == ack_id
                    && record.child_identity == Some(child_identity) =>
            {
                Ok(record.receipt)
            }
            Some(_) => Err(BrokerSpawnWalError::EffectIdentityConflict),
            None => Err(BrokerSpawnWalError::InvalidTransition),
        }
    }

    fn append_record_and_head(
        &mut self,
        phase: BrokerSpawnWalPhaseV1,
        operation_id: Uuid,
        attempt_id: Uuid,
        child_identity: Option<BrokerKernelChildIdentityV1>,
    ) -> Result<BrokerSpawnWalReceiptV1, BrokerSpawnWalError> {
        self.require_append_authority()?;
        let previous = self.records.last().copied();
        validate_broker_spawn_record_transition(previous, phase, attempt_id, child_identity)?;
        validate_broker_spawn_record_fields(phase, operation_id, attempt_id, child_identity)?;
        let sequence = u64::try_from(self.records.len())
            .map_err(|_| BrokerSpawnWalError::CapacityExhausted)?;
        let previous_record_mac =
            previous.map_or(self.header_mac, |record| record.receipt.record_mac);
        let wal_record = encode_broker_spawn_wal_record(
            self.header_mac,
            &self.authenticator,
            sequence,
            phase,
            operation_id,
            attempt_id,
            child_identity,
            previous_record_mac,
        )?;
        let record_mac = read_broker_array_32(&wal_record[144..176]);
        let head_record = encode_broker_spawn_head_record(
            self.head_header_mac,
            &self.authenticator,
            sequence,
            record_mac,
            self.terminal_head_mac,
        )?;
        let projected_wal_bytes = self
            .committed_wal_bytes
            .checked_add(BROKER_SPAWN_WAL_RECORD_BYTES_U64)
            .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
        let projected_head_bytes = self
            .committed_head_bytes
            .checked_add(BROKER_SPAWN_HEAD_RECORD_BYTES_U64)
            .ok_or(BrokerSpawnWalError::CapacityExhausted)?;
        if self.wal.metadata()?.len() != self.committed_wal_bytes
            || self.head.metadata()?.len() != self.committed_head_bytes
        {
            self.poisoned = true;
            return Err(BrokerSpawnWalError::ExternalLengthChange);
        }

        #[cfg(test)]
        if let Err(error) = self.fail_if_injected(BrokerSpawnWalInjectedFault::BeforeWalWrite) {
            return Err(BrokerSpawnWalError::Io(error));
        }

        let result = (|| -> Result<(), BrokerSpawnWalError> {
            self.wal.seek(SeekFrom::Start(self.committed_wal_bytes))?;
            self.wal.write_all(&wal_record)?;
            self.wal.sync_all()?;
            #[cfg(test)]
            self.fail_if_injected(BrokerSpawnWalInjectedFault::AfterWalSyncBeforeHead)?;
            self.head.seek(SeekFrom::Start(self.committed_head_bytes))?;
            self.head.write_all(&head_record)?;
            #[cfg(test)]
            self.fail_if_injected(BrokerSpawnWalInjectedFault::BeforeHeadSync)?;
            self.head.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.poisoned = true;
            return Err(error);
        }

        let head_mac = read_broker_array_32(&head_record[88..120]);
        let receipt = BrokerSpawnWalReceiptV1 {
            sequence,
            phase,
            committed_wal_bytes: projected_wal_bytes,
            committed_head_bytes: projected_head_bytes,
            record_mac,
            head_mac,
        };
        self.records.push(BrokerSpawnWalRecordState {
            phase,
            operation_id,
            attempt_id,
            child_identity,
            receipt,
        });
        self.committed_wal_bytes = projected_wal_bytes;
        self.committed_head_bytes = projected_head_bytes;
        self.terminal_head_mac = head_mac;
        Ok(receipt)
    }
}

impl BrokerSpawnAttemptPermitV1 {
    /// Invoke the one Spawn callback behind the synchronized Attempt fence.
    ///
    /// Callback error or recovered panic is conservatively indeterminate and
    /// never yields a replacement permit. If the callback succeeded but its
    /// child identity proof is invalid, the returned value is retained for
    /// quarantine rather than dropped accidentally.
    pub fn invoke_once<T, E>(
        self,
        effect: impl FnOnce() -> Result<(T, BrokerKernelChildIdentityV1), E>,
    ) -> BrokerSpawnAttemptExecutionV1<T> {
        match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(effect),
        ) {
            Ok(Ok((value, child_identity))) => {
                if child_identity.validate().is_err() {
                    BrokerSpawnAttemptExecutionV1::OutcomeIndeterminate {
                        retained_value: Some(value),
                    }
                } else {
                    BrokerSpawnAttemptExecutionV1::EffectSucceeded {
                        value,
                        observation: Box::new(BrokerSpawnObservationPermitV1 {
                            identity: self.identity,
                            attempt_id: self.attempt_id,
                            attempt_record_mac: self.attempt_record_mac,
                            child_identity,
                        }),
                    }
                }
            }
            Ok(Err(error)) => {
                let _ = catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| drop(error)),
                );
                BrokerSpawnAttemptExecutionV1::OutcomeIndeterminate {
                    retained_value: None,
                }
            }
            Err(_) => BrokerSpawnAttemptExecutionV1::OutcomeIndeterminate {
                retained_value: None,
            },
        }
    }
}

/// Hard per-pane limits checked before the broker allocates a PTY.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerResourceLimitsV1 {
    max_spawn_payload_bytes: usize,
    max_successor_handoffs: u32,
    max_proxy_operation_bytes: usize,
    max_buffered_output_bytes: usize,
}

impl BrokerResourceLimitsV1 {
    pub fn new(
        max_spawn_payload_bytes: usize,
        max_successor_handoffs: u32,
    ) -> Result<Self, BrokerError> {
        Self::with_proxy_bounds(
            max_spawn_payload_bytes,
            max_successor_handoffs,
            BROKER_DEFAULT_MAX_PROXY_OPERATION_BYTES,
            BROKER_DEFAULT_MAX_BUFFERED_OUTPUT_BYTES,
        )
    }

    pub fn with_proxy_bounds(
        max_spawn_payload_bytes: usize,
        max_successor_handoffs: u32,
        max_proxy_operation_bytes: usize,
        max_buffered_output_bytes: usize,
    ) -> Result<Self, BrokerError> {
        if max_spawn_payload_bytes == 0
            || max_spawn_payload_bytes > GUARDIAN_MAX_PAYLOAD_BYTES
            || max_successor_handoffs == 0
            || max_successor_handoffs > BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS
            || max_proxy_operation_bytes == 0
            || max_proxy_operation_bytes > BROKER_DEFAULT_MAX_PROXY_OPERATION_BYTES
            || max_buffered_output_bytes == 0
            || max_buffered_output_bytes > BROKER_ABSOLUTE_MAX_BUFFERED_OUTPUT_BYTES
        {
            return Err(BrokerError::CapacityExhausted);
        }
        Ok(Self {
            max_spawn_payload_bytes,
            max_successor_handoffs,
            max_proxy_operation_bytes,
            max_buffered_output_bytes,
        })
    }

    #[must_use]
    pub const fn max_spawn_payload_bytes(self) -> usize {
        self.max_spawn_payload_bytes
    }

    #[must_use]
    pub const fn max_successor_handoffs(self) -> u32 {
        self.max_successor_handoffs
    }

    #[must_use]
    pub const fn max_proxy_operation_bytes(self) -> usize {
        self.max_proxy_operation_bytes
    }

    #[must_use]
    pub const fn max_buffered_output_bytes(self) -> usize {
        self.max_buffered_output_bytes
    }
}

impl Default for BrokerResourceLimitsV1 {
    fn default() -> Self {
        Self {
            max_spawn_payload_bytes: GUARDIAN_MAX_PAYLOAD_BYTES,
            max_successor_handoffs: BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS,
            max_proxy_operation_bytes: BROKER_DEFAULT_MAX_PROXY_OPERATION_BYTES,
            max_buffered_output_bytes: BROKER_DEFAULT_MAX_BUFFERED_OUTPUT_BYTES,
        }
    }
}

/// Content-free accounting for every handle retained or issued for one pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerResourceUsageV1 {
    pub broker_pty_descriptors: u8,
    pub child_handles: u8,
    pub live_guardian_leases: u8,
    pub retained_spawn_payload_bytes: u64,
    pub buffered_output_bytes: usize,
    pub max_buffered_output_bytes: usize,
    pub fenced_attachment_tombstones: usize,
    pub max_fenced_attachment_tombstones: usize,
    pub completed_successor_handoffs: u32,
}

/// Process-local content-free identity of the child admitted for a reservation.
///
/// The nonce prevents accidental in-process aliasing; PID plus nonce is not a
/// non-recycled cross-process identity. Activated recovery must bind a pidfd or
/// platform-equivalent kernel start identity in the durable broker Spawn/Ack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerChildIdentityV1 {
    pub durable_pane_id: Uuid,
    pub spawn_effect_id: Uuid,
    pub process_id: u32,
    pub broker_child_nonce: Uuid,
}

/// Stable identity of one guardian attachment lease.
///
/// Copying this identity does not copy descriptor or protocol authority.  The
/// non-Clone EOF and handoff values below are the corresponding authorities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerAttachmentIdentityV1 {
    broker_incarnation: Uuid,
    durable_pane_id: Uuid,
    spawn_effect_id: Uuid,
    attachment_id: Uuid,
    owner: BrokerGuardianOwnerIdentity,
    lease_generation: u64,
}

impl BrokerAttachmentIdentityV1 {
    #[must_use]
    pub const fn durable_pane_id(self) -> Uuid {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn attachment_id(self) -> Uuid {
        self.attachment_id
    }

    #[must_use]
    pub const fn lease_generation(self) -> u64 {
        self.lease_generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrokerGuardianOwnerIdentity {
    guardian_incarnation: Uuid,
    connection_id: Uuid,
    mux_incarnation: Uuid,
    mux_build_identity_digest: [u8; 32],
    guardian_build_identity_digest: [u8; 32],
}

/// Nonduplicable evidence produced only after the broker transport has
/// authenticated both the connection and its sealed process-family builds.
pub struct BrokerAuthenticatedGuardianConnectionV1 {
    broker_incarnation: Uuid,
    owner: BrokerGuardianOwnerIdentity,
}

impl std::fmt::Debug for BrokerAuthenticatedGuardianConnectionV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerAuthenticatedGuardianConnectionV1")
            .field("broker_incarnation", &self.broker_incarnation)
            .field("guardian_incarnation", &self.owner.guardian_incarnation)
            .field("connection_id", &self.owner.connection_id)
            .field("mux_incarnation", &self.owner.mux_incarnation)
            .field("mux_build_identity_digest", &"[REDACTED]")
            .field("guardian_build_identity_digest", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl BrokerAuthenticatedGuardianConnectionV1 {
    /// Future broker transport integration seam.
    ///
    /// Calling this function is valid only after the transport has verified
    /// the peer MAC, connection nonce, guardian incarnation, mux incarnation,
    /// and both sealed build identities.  It is crate-private so decoded wire
    /// fields cannot become authority outside the trusted guardian boundary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_authenticated_transport(
        broker_incarnation: Uuid,
        guardian_incarnation: Uuid,
        connection_id: Uuid,
        mux_incarnation: Uuid,
        mux_build_identity: SealedAtomicBuildIdentity,
        guardian_build_identity: SealedAtomicBuildIdentity,
    ) -> Result<Self, BrokerError> {
        let owner = BrokerGuardianOwnerIdentity {
            guardian_incarnation,
            connection_id,
            mux_incarnation,
            mux_build_identity_digest: mux_build_identity.into_bytes(),
            guardian_build_identity_digest: guardian_build_identity.into_bytes(),
        };
        if broker_incarnation.is_nil() || !owner.is_valid() {
            return Err(BrokerError::InvalidAuthenticatedAuthority);
        }
        Ok(Self {
            broker_incarnation,
            owner,
        })
    }
}

impl BrokerGuardianOwnerIdentity {
    fn is_valid(self) -> bool {
        !self.guardian_incarnation.is_nil()
            && !self.connection_id.is_nil()
            && !self.mux_incarnation.is_nil()
            && self.mux_build_identity_digest != [0; 32]
            && self.guardian_build_identity_digest != [0; 32]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrokerGenesisBinding {
    mux_incarnation: Uuid,
    spawn_effect_id: Uuid,
    durable_pane_id: Uuid,
    origin_request_id: Uuid,
    spawn_payload_bytes: u64,
    spawn_payload_digest: [u8; 32],
    spawning_mux_build_identity_digest: [u8; 32],
    live_guardian_build_identity_digest: [u8; 32],
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
    checkpoint_identity_digest: [u8; 32],
    boundary_identity_digest: [u8; 32],
    upload_id: Uuid,
}

impl From<&GuardianGenesisReservationIdentityV1> for BrokerGenesisBinding {
    fn from(identity: &GuardianGenesisReservationIdentityV1) -> Self {
        Self {
            mux_incarnation: identity.mux_incarnation(),
            spawn_effect_id: identity.spawn_effect_id(),
            durable_pane_id: identity.durable_pane_id(),
            origin_request_id: identity.origin_request_id(),
            spawn_payload_bytes: identity.spawn_payload_bytes(),
            spawn_payload_digest: identity.spawn_payload_digest(),
            spawning_mux_build_identity_digest: identity.spawning_mux_build_identity_digest(),
            live_guardian_build_identity_digest: identity.live_guardian_build_identity_digest(),
            rows: identity.rows(),
            cols: identity.cols(),
            pixel_width: identity.pixel_width(),
            pixel_height: identity.pixel_height(),
            checkpoint_identity_digest: identity.checkpoint_identity_digest(),
            boundary_identity_digest: identity.boundary_identity_digest(),
            upload_id: identity.upload_id(),
        }
    }
}

impl BrokerGenesisBinding {
    fn validate(self) -> Result<(), BrokerError> {
        if self.mux_incarnation.is_nil()
            || self.spawn_effect_id.is_nil()
            || self.durable_pane_id.is_nil()
            || self.origin_request_id.is_nil()
            || self.spawn_payload_bytes == 0
            || self.spawn_payload_digest == [0; 32]
            || self.spawning_mux_build_identity_digest == [0; 32]
            || self.live_guardian_build_identity_digest == [0; 32]
            || self.rows == 0
            || self.cols == 0
            || self.checkpoint_identity_digest == [0; 32]
            || self.boundary_identity_digest == [0; 32]
            || self.upload_id.is_nil()
        {
            return Err(BrokerError::InvalidGenesisReservation);
        }
        Ok(())
    }

    const fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
        }
    }

    fn encode(self) -> [u8; BROKER_GENESIS_BINDING_BYTES] {
        let mut bytes = [0_u8; BROKER_GENESIS_BINDING_BYTES];
        bytes[0..16].copy_from_slice(self.mux_incarnation.as_bytes());
        bytes[16..32].copy_from_slice(self.spawn_effect_id.as_bytes());
        bytes[32..48].copy_from_slice(self.durable_pane_id.as_bytes());
        bytes[48..64].copy_from_slice(self.origin_request_id.as_bytes());
        bytes[64..72].copy_from_slice(&self.spawn_payload_bytes.to_be_bytes());
        bytes[72..104].copy_from_slice(&self.spawn_payload_digest);
        bytes[104..136].copy_from_slice(&self.spawning_mux_build_identity_digest);
        bytes[136..168].copy_from_slice(&self.live_guardian_build_identity_digest);
        bytes[168..170].copy_from_slice(&self.rows.to_be_bytes());
        bytes[170..172].copy_from_slice(&self.cols.to_be_bytes());
        bytes[172..174].copy_from_slice(&self.pixel_width.to_be_bytes());
        bytes[174..176].copy_from_slice(&self.pixel_height.to_be_bytes());
        bytes[176..208].copy_from_slice(&self.checkpoint_identity_digest);
        bytes[208..240].copy_from_slice(&self.boundary_identity_digest);
        bytes[240..256].copy_from_slice(self.upload_id.as_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, BrokerControlProtocolError> {
        if bytes.len() != BROKER_GENESIS_BINDING_BYTES {
            return Err(BrokerControlProtocolError::InvalidLength);
        }
        let binding = Self {
            mux_incarnation: read_broker_uuid(&bytes[0..16]),
            spawn_effect_id: read_broker_uuid(&bytes[16..32]),
            durable_pane_id: read_broker_uuid(&bytes[32..48]),
            origin_request_id: read_broker_uuid(&bytes[48..64]),
            spawn_payload_bytes: read_broker_be_u64(&bytes[64..72]),
            spawn_payload_digest: read_broker_array_32(&bytes[72..104]),
            spawning_mux_build_identity_digest: read_broker_array_32(&bytes[104..136]),
            live_guardian_build_identity_digest: read_broker_array_32(&bytes[136..168]),
            rows: read_broker_be_u16(&bytes[168..170]),
            cols: read_broker_be_u16(&bytes[170..172]),
            pixel_width: read_broker_be_u16(&bytes[172..174]),
            pixel_height: read_broker_be_u16(&bytes[174..176]),
            checkpoint_identity_digest: read_broker_array_32(&bytes[176..208]),
            boundary_identity_digest: read_broker_array_32(&bytes[208..240]),
            upload_id: read_broker_uuid(&bytes[240..256]),
        };
        binding
            .validate()
            .map_err(|_| BrokerControlProtocolError::InvalidIdentity)?;
        Ok(binding)
    }

    fn matches_control_header(self, header: BrokerControlRequestHeaderV1) -> bool {
        let BrokerControlRequestHeaderV1 {
            mux_incarnation,
            operation_id: requested_spawn_effect_id,
            durable_pane_id,
            mux_build_identity_digest,
            guardian_build_identity_digest,
            ..
        } = header;
        let effect_identity_matches = self.spawn_effect_id == requested_spawn_effect_id
            && self.durable_pane_id == durable_pane_id;
        let build_identity_matches = self.spawning_mux_build_identity_digest
            == mux_build_identity_digest
            && self.live_guardian_build_identity_digest == guardian_build_identity_digest;
        self.mux_incarnation == mux_incarnation && effect_identity_matches && build_identity_matches
    }
}

/// Complete, authenticated Spawn admission carried over the broker-control
/// channel. The production constructor consumes the guardian's already
/// durable pre-Spawn permit; the broker independently revalidates its exact
/// binding before allocating a PTY or appending its own WAL.
pub(crate) struct BrokerSpawnControlRequestV1 {
    journal_id: Uuid,
    attempt_id: Uuid,
    catalog_candidate_checksum: [u8; BROKER_CATALOG_CHECKSUM_BYTES],
    binding: BrokerGenesisBinding,
    payload: GuardianSpawnPayload,
}

impl BrokerSpawnControlRequestV1 {
    pub(crate) fn from_published_admission(
        permit: GuardianPublishedGenesisAdmissionPermitV1,
        payload: GuardianSpawnPayload,
        journal_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<Self, BrokerControlProtocolError> {
        let proof = BrokerDurablePreSpawnIntentProof::from_permit(permit);
        Self::from_parts(
            journal_id,
            attempt_id,
            proof.catalog_candidate_checksum,
            proof.binding,
            payload,
        )
    }

    fn from_parts(
        journal_id: Uuid,
        attempt_id: Uuid,
        catalog_candidate_checksum: [u8; BROKER_CATALOG_CHECKSUM_BYTES],
        binding: BrokerGenesisBinding,
        payload: GuardianSpawnPayload,
    ) -> Result<Self, BrokerControlProtocolError> {
        let request = Self {
            journal_id,
            attempt_id,
            catalog_candidate_checksum,
            binding,
            payload,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), BrokerControlProtocolError> {
        if self.journal_id.is_nil()
            || self.attempt_id.is_nil()
            || self.catalog_candidate_checksum == [0; BROKER_CATALOG_CHECKSUM_BYTES]
        {
            return Err(BrokerControlProtocolError::InvalidIdentity);
        }
        self.binding
            .validate()
            .map_err(|_| BrokerControlProtocolError::InvalidIdentity)?;
        let canonical = self
            .payload
            .encode()
            .map_err(|_| BrokerControlProtocolError::InvalidShape)?;
        let canonical_bytes = u64::try_from(canonical.len())
            .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
        if canonical.is_empty()
            || canonical.len() > BROKER_CONTROL_MAX_PAYLOAD_BYTES - BROKER_SPAWN_CONTROL_FIXED_BYTES
            || canonical_bytes != self.binding.spawn_payload_bytes
            || <[u8; 32]>::from(Sha256::digest(canonical.as_slice()))
                != self.binding.spawn_payload_digest
            || self.payload.size() != self.binding.pty_size()
        {
            return Err(BrokerControlProtocolError::InvalidShape);
        }
        Ok(())
    }

    fn validate_control_header(
        &self,
        header: BrokerControlRequestHeaderV1,
    ) -> Result<(), BrokerControlProtocolError> {
        if header.operation != BrokerControlOperationV1::Spawn
            || !self.binding.matches_control_header(header)
        {
            return Err(BrokerControlProtocolError::InvalidIdentity);
        }
        Ok(())
    }

    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, BrokerControlProtocolError> {
        self.validate()?;
        let canonical = self
            .payload
            .encode()
            .map_err(|_| BrokerControlProtocolError::InvalidShape)?;
        let total_bytes = BROKER_SPAWN_CONTROL_FIXED_BYTES
            .checked_add(canonical.len())
            .ok_or(BrokerControlProtocolError::CapacityExhausted)?;
        if total_bytes > BROKER_CONTROL_MAX_PAYLOAD_BYTES {
            return Err(BrokerControlProtocolError::CapacityExhausted);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        bytes
            .try_reserve_exact(total_bytes)
            .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
        bytes.resize(BROKER_SPAWN_CONTROL_FIXED_BYTES, 0);
        bytes[0..4].copy_from_slice(&BROKER_SPAWN_CONTROL_MAGIC);
        bytes[4..6].copy_from_slice(&BROKER_CONTROL_VERSION.to_be_bytes());
        bytes[6..8].fill(0);
        bytes[8..24].copy_from_slice(self.journal_id.as_bytes());
        bytes[24..40].copy_from_slice(self.attempt_id.as_bytes());
        bytes[40..72].copy_from_slice(&self.catalog_candidate_checksum);
        bytes[72..328].copy_from_slice(&self.binding.encode());
        let payload_bytes = u32::try_from(canonical.len())
            .map_err(|_| BrokerControlProtocolError::CapacityExhausted)?;
        bytes[328..332].copy_from_slice(&payload_bytes.to_be_bytes());
        bytes.extend_from_slice(&canonical);
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, BrokerControlProtocolError> {
        if bytes.len() < BROKER_SPAWN_CONTROL_FIXED_BYTES
            || bytes.len() > BROKER_CONTROL_MAX_PAYLOAD_BYTES
            || bytes[0..4] != BROKER_SPAWN_CONTROL_MAGIC
            || read_broker_be_u16(&bytes[4..6]) != BROKER_CONTROL_VERSION
            || bytes[6..8] != [0, 0]
        {
            return Err(BrokerControlProtocolError::InvalidLength);
        }
        let payload_bytes = usize::try_from(read_broker_be_u32(&bytes[328..332]))
            .map_err(|_| BrokerControlProtocolError::InvalidLength)?;
        if BROKER_SPAWN_CONTROL_FIXED_BYTES.checked_add(payload_bytes) != Some(bytes.len()) {
            return Err(BrokerControlProtocolError::InvalidLength);
        }
        let payload = GuardianSpawnPayload::decode(&bytes[BROKER_SPAWN_CONTROL_FIXED_BYTES..])
            .map_err(|_| BrokerControlProtocolError::InvalidShape)?;
        Self::from_parts(
            read_broker_uuid(&bytes[8..24]),
            read_broker_uuid(&bytes[24..40]),
            read_broker_array_32(&bytes[40..72]),
            BrokerGenesisBinding::decode(&bytes[72..328])?,
            payload,
        )
    }
}

impl std::fmt::Debug for BrokerSpawnControlRequestV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerSpawnControlRequestV1")
            .field("journal_id", &self.journal_id)
            .field("attempt_id", &self.attempt_id)
            .field("durable_pane_id", &self.binding.durable_pane_id)
            .field("spawn_effect_id", &self.binding.spawn_effect_id)
            .field("catalog_candidate_checksum", &"[REDACTED]")
            .field("spawn_payload", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Nonduplicable authority over one prepared, child-free PTY reservation.
pub struct BrokerControlLeaseV1 {
    broker_incarnation: Uuid,
    durable_pane_id: Uuid,
    spawn_effect_id: Uuid,
    control_id: Uuid,
    owner: BrokerGuardianOwnerIdentity,
}

impl std::fmt::Debug for BrokerControlLeaseV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerControlLeaseV1")
            .field("broker_incarnation", &self.broker_incarnation)
            .field("durable_pane_id", &self.durable_pane_id)
            .field("spawn_effect_id", &self.spawn_effect_id)
            .field("control_id", &self.control_id)
            .field("owner", &"[AUTHENTICATED]")
            .finish_non_exhaustive()
    }
}

/// Child-free broker typestate after PTY allocation and before durable
/// adoption.  Dropping or aborting this value cannot leave a user child.
pub struct BrokerPreparedPaneV1 {
    slave: Box<dyn portable_pty::SlavePty + Send>,
    broker_master: Box<dyn MasterPty + Send>,
    proxy_reader: Box<dyn PollablePtyReader>,
    proxy_writer: Box<dyn Write + Send>,
    output_buffer: VecDeque<u8>,
    fenced_attachments: VecDeque<BrokerAttachmentIdentityV1>,
    command: portable_pty::CommandBuilder,
    binding: BrokerGenesisBinding,
    broker_incarnation: Uuid,
    control_id: Uuid,
    initial_owner: BrokerGuardianOwnerIdentity,
    initial_attachment_id: Uuid,
    limits: BrokerResourceLimitsV1,
}

impl BrokerPreparedPaneV1 {
    /// Prepare a child-free PTY from one authenticated Genesis reservation.
    ///
    /// The canonical Spawn payload, complete geometry, mux build, guardian
    /// build, and mux incarnation are verified before `openpty`. The broker's
    /// sole master, sole reader, and sole byte-silent writer are allocated
    /// before this returns. None is transferred to the guardian.
    pub(crate) fn prepare(
        reservation: &GuardianGenesisReservationIdentityV1,
        payload: GuardianSpawnPayload,
        authority: &BrokerAuthenticatedGuardianConnectionV1,
        limits: BrokerResourceLimitsV1,
    ) -> Result<(Self, BrokerControlLeaseV1), BrokerError> {
        Self::prepare_binding(
            BrokerGenesisBinding::from(reservation),
            payload,
            authority,
            limits,
        )
    }

    fn prepare_binding(
        binding: BrokerGenesisBinding,
        payload: GuardianSpawnPayload,
        authority: &BrokerAuthenticatedGuardianConnectionV1,
        limits: BrokerResourceLimitsV1,
    ) -> Result<(Self, BrokerControlLeaseV1), BrokerError> {
        binding.validate()?;
        if authority.broker_incarnation.is_nil() || !authority.owner.is_valid() {
            return Err(BrokerError::InvalidAuthenticatedAuthority);
        }
        if authority.owner.mux_incarnation != binding.mux_incarnation
            || authority.owner.mux_build_identity_digest
                != binding.spawning_mux_build_identity_digest
            || authority.owner.guardian_build_identity_digest
                != binding.live_guardian_build_identity_digest
        {
            return Err(BrokerError::AuthenticatedAuthorityMismatch);
        }

        let canonical_payload = payload
            .encode()
            .map_err(|_| BrokerError::InvalidSpawnPayload)?;
        let payload_bytes =
            u64::try_from(canonical_payload.len()).map_err(|_| BrokerError::CapacityExhausted)?;
        let payload_digest: [u8; 32] = Sha256::digest(canonical_payload.as_slice()).into();
        if canonical_payload.len() > limits.max_spawn_payload_bytes
            || payload_bytes != binding.spawn_payload_bytes
            || payload_digest != binding.spawn_payload_digest
            || payload.size() != binding.pty_size()
        {
            return Err(BrokerError::SpawnPayloadBindingMismatch);
        }
        drop(canonical_payload);

        let control_id = Uuid::new_v4();
        let initial_attachment_id = Uuid::new_v4();
        let (command, size) = payload.into_parts();
        let PtyPair {
            slave,
            master: broker_master,
        } = native_pty_system()
            .openpty(size)
            .map_err(|_| BrokerError::PtyAllocationFailed)?;
        let proxy_reader = broker_master
            .try_clone_pollable_reader()
            .map_err(|_| BrokerError::ProxyIoPreparationFailed)?;
        let proxy_writer = broker_master
            .take_writer_for_broker_proxy()
            .map_err(|_| BrokerError::ProxyIoPreparationFailed)?;
        let mut output_buffer = VecDeque::new();
        output_buffer
            .try_reserve_exact(limits.max_buffered_output_bytes)
            .map_err(|_| BrokerError::CapacityExhausted)?;
        let max_fenced_attachment_tombstones = usize::try_from(limits.max_successor_handoffs)
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(BrokerError::CapacityExhausted)?;
        let mut fenced_attachments = VecDeque::new();
        fenced_attachments
            .try_reserve_exact(max_fenced_attachment_tombstones)
            .map_err(|_| BrokerError::CapacityExhausted)?;
        let control = BrokerControlLeaseV1 {
            broker_incarnation: authority.broker_incarnation,
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            control_id,
            owner: authority.owner,
        };
        Ok((
            Self {
                slave,
                broker_master,
                proxy_reader,
                proxy_writer,
                output_buffer,
                fenced_attachments,
                command,
                binding,
                broker_incarnation: authority.broker_incarnation,
                control_id,
                initial_owner: authority.owner,
                initial_attachment_id,
                limits,
            },
            control,
        ))
    }

    #[must_use]
    pub fn resource_usage(&self) -> BrokerResourceUsageV1 {
        BrokerResourceUsageV1 {
            broker_pty_descriptors: 3,
            child_handles: 0,
            live_guardian_leases: 0,
            retained_spawn_payload_bytes: self.binding.spawn_payload_bytes,
            buffered_output_bytes: 0,
            max_buffered_output_bytes: self.limits.max_buffered_output_bytes,
            fenced_attachment_tombstones: 0,
            max_fenced_attachment_tombstones: self.max_fenced_attachment_tombstones(),
            completed_successor_handoffs: 0,
        }
    }

    /// Consume a prepared reservation after authenticated control EOF.
    ///
    /// Since the only child-creating operation exists on the durable commit
    /// path, this transition deterministically leaves no user child.
    pub fn abort_after_authenticated_control_eof(
        self,
        control: BrokerControlLeaseV1,
    ) -> Result<BrokerPreAdoptionAbortReceiptV1, BrokerError> {
        self.validate_control(&control)?;
        Ok(BrokerPreAdoptionAbortReceiptV1 {
            broker_incarnation: self.broker_incarnation,
            durable_pane_id: self.binding.durable_pane_id,
            spawn_effect_id: self.binding.spawn_effect_id,
        })
    }

    /// Consume durable Genesis pre-Spawn intent and process-locally spawn once.
    ///
    /// This is not a crash-durable child-exists acknowledgement. The activated
    /// broker must surround this cut with its own durable Spawn/Ack log and
    /// startup reconciliation before it may retry.
    pub(crate) fn commit_after_durable_pre_spawn_intent(
        self,
        control: BrokerControlLeaseV1,
        permit: GuardianPublishedGenesisAdmissionPermitV1,
    ) -> Result<BrokerAdoptionV1, BrokerError> {
        let proof = BrokerDurablePreSpawnIntentProof::from_permit(permit);
        self.commit_with_proof(control, &proof)
    }

    fn commit_with_proof(
        self,
        control: BrokerControlLeaseV1,
        proof: &BrokerDurablePreSpawnIntentProof,
    ) -> Result<BrokerAdoptionV1, BrokerError> {
        self.validate_control(&control)?;
        if proof.binding != self.binding {
            return Err(BrokerError::DurablePreSpawnIntentMismatch);
        }
        if proof.catalog_candidate_checksum == [0; BROKER_CATALOG_CHECKSUM_BYTES] {
            return Err(BrokerError::InvalidCatalogChecksum);
        }

        // Generate every broker identity before spawning.  After the spawn
        // succeeds, constructing the returned typestate performs no fallible
        // allocation or descriptor operation.
        let broker_child_nonce = Uuid::new_v4();
        let mut child = self
            .slave
            .spawn_command(self.command)
            .map_err(|_| BrokerError::ChildSpawnFailed)?;
        drop(self.slave);
        let Some(process_id) = child.process_id() else {
            // Native Unix children always expose a PID.  If a backend violates
            // that contract, synchronously reap the just-created child and
            // refuse to publish an ambiguous identity.
            let _ = child.kill();
            let _ = child.wait();
            return Err(BrokerError::ChildIdentityUnavailable);
        };
        let child_identity = BrokerChildIdentityV1 {
            durable_pane_id: self.binding.durable_pane_id,
            spawn_effect_id: self.binding.spawn_effect_id,
            process_id,
            broker_child_nonce,
        };
        let attachment_identity = BrokerAttachmentIdentityV1 {
            broker_incarnation: self.broker_incarnation,
            durable_pane_id: self.binding.durable_pane_id,
            spawn_effect_id: self.binding.spawn_effect_id,
            attachment_id: self.initial_attachment_id,
            owner: self.initial_owner,
            lease_generation: 1,
        };
        let attachment = BrokerPtyAttachmentV1 {
            identity: attachment_identity,
        };
        let pane = BrokerAdoptedPaneV1 {
            broker_incarnation: self.broker_incarnation,
            binding: self.binding,
            broker_master: self.broker_master,
            proxy_reader: Some(self.proxy_reader),
            proxy_writer: Some(self.proxy_writer),
            output_buffer: self.output_buffer,
            fenced_attachments: self.fenced_attachments,
            child,
            child_identity,
            limits: self.limits,
            completed_successor_handoffs: 0,
            buffer_start_sequence: 0,
            next_output_sequence: 0,
            current_lease_output_cursor: 0,
            output_terminal: None,
            lease: BrokerLeaseState::Active {
                attachment: attachment_identity,
                last_handoff_id: None,
            },
        };
        Ok(BrokerAdoptionV1 { pane, attachment })
    }

    fn validate_control(&self, control: &BrokerControlLeaseV1) -> Result<(), BrokerError> {
        if control.broker_incarnation != self.broker_incarnation
            || control.durable_pane_id != self.binding.durable_pane_id
            || control.spawn_effect_id != self.binding.spawn_effect_id
            || control.control_id != self.control_id
            || control.owner != self.initial_owner
        {
            return Err(BrokerError::ControlLeaseMismatch);
        }
        Ok(())
    }

    fn max_fenced_attachment_tombstones(&self) -> usize {
        usize::try_from(self.limits.max_successor_handoffs)
            .expect("validated successor handoff limit fits usize")
            + 1
    }
}

#[derive(Debug, Eq, PartialEq)]
struct BrokerDurablePreSpawnIntentProof {
    binding: BrokerGenesisBinding,
    catalog_candidate_checksum: [u8; BROKER_CATALOG_CHECKSUM_BYTES],
}

impl BrokerDurablePreSpawnIntentProof {
    fn from_permit(permit: GuardianPublishedGenesisAdmissionPermitV1) -> Self {
        let catalog_candidate_checksum = *permit.catalog_candidate_checksum();
        // This consumes the catalog's one-way pre-Spawn intent. It does not
        // assert that a child exists; the future broker WAL supplies that fact.
        let reservation_identity = permit.into_reservation_identity();
        let binding = BrokerGenesisBinding::from(&reservation_identity);
        Self {
            binding,
            catalog_candidate_checksum,
        }
    }
}

/// Content-free proof that a prepared PTY was dropped before any child spawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerPreAdoptionAbortReceiptV1 {
    pub broker_incarnation: Uuid,
    pub durable_pane_id: Uuid,
    pub spawn_effect_id: Uuid,
}

/// Process-local result of the sole child-creating transition.
///
/// This value is not a durable Spawn acknowledgement; see the module-level
/// crash-cut boundary.
pub struct BrokerAdoptionV1 {
    pub pane: BrokerAdoptedPaneV1,
    pub attachment: BrokerPtyAttachmentV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrokerLeaseState {
    Active {
        attachment: BrokerAttachmentIdentityV1,
        last_handoff_id: Option<Uuid>,
    },
    AwaitingSuccessor {
        predecessor: BrokerAttachmentIdentityV1,
        next_generation: u64,
    },
    Quarantined {
        reason: BrokerQuarantineReasonV1,
        attachment_may_be_open: bool,
    },
    FinalTerminalEof {
        predecessor: BrokerAttachmentIdentityV1,
        generation: u64,
    },
    FinalTerminalEofPending {
        predecessor: BrokerAttachmentIdentityV1,
        generation: u64,
    },
}

/// Sticky reason that prevents an ambiguous pane from rotating or spawning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerQuarantineReasonV1 {
    ConflictingControlEof,
    ConflictingSuccessorHandoff,
    LeaseGenerationExhausted,
    HandoffCapacityExhausted,
    FencedAttachmentCapacityExhausted,
}

/// Content-free query state for lost-reply recovery and operator inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerPaneLifecycleV1 {
    Active,
    AwaitingSuccessor,
    Quarantined(BrokerQuarantineReasonV1),
    FinalTerminalEof,
    FinalTerminalEofPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerPaneStatusV1 {
    pub broker_incarnation: Uuid,
    pub child_identity: BrokerChildIdentityV1,
    pub lifecycle: BrokerPaneLifecycleV1,
    pub lease_generation: u64,
    pub owner_guardian_incarnation: Option<Uuid>,
    pub owner_mux_incarnation: Option<Uuid>,
    pub owner_guardian_build_identity_digest: Option<[u8; 32]>,
    pub owner_mux_build_identity_digest: Option<[u8; 32]>,
    pub completed_successor_handoffs: u32,
    pub output_sequence: u64,
    pub output_terminal_reason: Option<BrokerOutputTerminalReasonV1>,
    pub output_child_exit_observed: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerProxyOperationKindV1 {
    Read,
    Write,
    Resize,
    AcknowledgeOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrokerProxyOperationV1 {
    Read { max_bytes: usize },
    Write { bytes: usize, digest: [u8; 32] },
    Resize { size: PtySize },
    AcknowledgeOutput { through_sequence: u64 },
}

/// Nonduplicable admission for one bounded proxy operation.
///
/// A queued operation carries the exact attachment identity and generation.
/// Execution revalidates both immediately before touching the PTY, so a lease
/// rotation fences already-admitted stale work as well as future requests.
pub struct BrokerProxyOperationPermitV1 {
    operation_id: Uuid,
    attachment: BrokerAttachmentIdentityV1,
    operation: BrokerProxyOperationV1,
}

impl std::fmt::Debug for BrokerProxyOperationPermitV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerProxyOperationPermitV1")
            .field("operation_id", &self.operation_id)
            .field("attachment", &self.attachment)
            .field("operation", &self.operation.kind())
            .finish_non_exhaustive()
    }
}

impl BrokerProxyOperationV1 {
    const fn kind(self) -> BrokerProxyOperationKindV1 {
        match self {
            Self::Read { .. } => BrokerProxyOperationKindV1::Read,
            Self::Write { .. } => BrokerProxyOperationKindV1::Write,
            Self::Resize { .. } => BrokerProxyOperationKindV1::Resize,
            Self::AcknowledgeOutput { .. } => BrokerProxyOperationKindV1::AcknowledgeOutput,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerProxyEffectReceiptV1 {
    pub operation_id: Uuid,
    pub lease_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerProxyReadReceiptV1 {
    pub operation_id: Uuid,
    pub lease_generation: u64,
    pub output_sequence_start: u64,
    pub output_sequence_end: u64,
    pub bytes_read: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerOutputPumpReceiptV1 {
    pub output_sequence_start: u64,
    pub output_sequence_end: u64,
    pub bytes_drained: usize,
    pub buffered_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerOutputTerminalReasonV1 {
    ZeroLengthRead,
    PtyIoClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrokerOutputTerminalReceiptV1 {
    pub reason: BrokerOutputTerminalReasonV1,
    pub output_sequence: u64,
    pub buffered_bytes: usize,
    pub child_exit_observed: Option<bool>,
    pub newly_observed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerOutputPumpOutcomeV1 {
    Drained(BrokerOutputPumpReceiptV1),
    TerminalDrained(BrokerOutputTerminalReceiptV1),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BrokerOutputTerminalState {
    reason: BrokerOutputTerminalReasonV1,
    child_exit_observed: Option<bool>,
}

/// Broker-retained PTY master and exact child handle after durable adoption.
pub struct BrokerAdoptedPaneV1 {
    broker_incarnation: Uuid,
    binding: BrokerGenesisBinding,
    broker_master: Box<dyn MasterPty + Send>,
    proxy_reader: Option<Box<dyn PollablePtyReader>>,
    proxy_writer: Option<Box<dyn Write + Send>>,
    output_buffer: VecDeque<u8>,
    fenced_attachments: VecDeque<BrokerAttachmentIdentityV1>,
    child: Box<dyn Child + Send + Sync>,
    child_identity: BrokerChildIdentityV1,
    limits: BrokerResourceLimitsV1,
    completed_successor_handoffs: u32,
    buffer_start_sequence: u64,
    next_output_sequence: u64,
    current_lease_output_cursor: u64,
    output_terminal: Option<BrokerOutputTerminalState>,
    lease: BrokerLeaseState,
}

impl BrokerAdoptedPaneV1 {
    #[must_use]
    pub const fn child_identity(&self) -> BrokerChildIdentityV1 {
        self.child_identity
    }

    #[must_use]
    pub fn status(&self) -> BrokerPaneStatusV1 {
        match self.lease {
            BrokerLeaseState::Active { attachment, .. } => BrokerPaneStatusV1 {
                broker_incarnation: self.broker_incarnation,
                child_identity: self.child_identity,
                lifecycle: BrokerPaneLifecycleV1::Active,
                lease_generation: attachment.lease_generation,
                owner_guardian_incarnation: Some(attachment.owner.guardian_incarnation),
                owner_mux_incarnation: Some(attachment.owner.mux_incarnation),
                owner_guardian_build_identity_digest: Some(
                    attachment.owner.guardian_build_identity_digest,
                ),
                owner_mux_build_identity_digest: Some(attachment.owner.mux_build_identity_digest),
                completed_successor_handoffs: self.completed_successor_handoffs,
                output_sequence: self.next_output_sequence,
                output_terminal_reason: self.output_terminal.map(|state| state.reason),
                output_child_exit_observed: self
                    .output_terminal
                    .and_then(|state| state.child_exit_observed),
            },
            BrokerLeaseState::AwaitingSuccessor {
                next_generation, ..
            } => BrokerPaneStatusV1 {
                broker_incarnation: self.broker_incarnation,
                child_identity: self.child_identity,
                lifecycle: BrokerPaneLifecycleV1::AwaitingSuccessor,
                lease_generation: next_generation,
                owner_guardian_incarnation: None,
                owner_mux_incarnation: None,
                owner_guardian_build_identity_digest: None,
                owner_mux_build_identity_digest: None,
                completed_successor_handoffs: self.completed_successor_handoffs,
                output_sequence: self.next_output_sequence,
                output_terminal_reason: self.output_terminal.map(|state| state.reason),
                output_child_exit_observed: self
                    .output_terminal
                    .and_then(|state| state.child_exit_observed),
            },
            BrokerLeaseState::Quarantined { reason, .. } => BrokerPaneStatusV1 {
                broker_incarnation: self.broker_incarnation,
                child_identity: self.child_identity,
                lifecycle: BrokerPaneLifecycleV1::Quarantined(reason),
                lease_generation: self.current_generation(),
                owner_guardian_incarnation: None,
                owner_mux_incarnation: None,
                owner_guardian_build_identity_digest: None,
                owner_mux_build_identity_digest: None,
                completed_successor_handoffs: self.completed_successor_handoffs,
                output_sequence: self.next_output_sequence,
                output_terminal_reason: self.output_terminal.map(|state| state.reason),
                output_child_exit_observed: self
                    .output_terminal
                    .and_then(|state| state.child_exit_observed),
            },
            BrokerLeaseState::FinalTerminalEof { generation, .. } => BrokerPaneStatusV1 {
                broker_incarnation: self.broker_incarnation,
                child_identity: self.child_identity,
                lifecycle: BrokerPaneLifecycleV1::FinalTerminalEof,
                lease_generation: generation,
                owner_guardian_incarnation: None,
                owner_mux_incarnation: None,
                owner_guardian_build_identity_digest: None,
                owner_mux_build_identity_digest: None,
                completed_successor_handoffs: self.completed_successor_handoffs,
                output_sequence: self.next_output_sequence,
                output_terminal_reason: self.output_terminal.map(|state| state.reason),
                output_child_exit_observed: self
                    .output_terminal
                    .and_then(|state| state.child_exit_observed),
            },
            BrokerLeaseState::FinalTerminalEofPending { generation, .. } => BrokerPaneStatusV1 {
                broker_incarnation: self.broker_incarnation,
                child_identity: self.child_identity,
                lifecycle: BrokerPaneLifecycleV1::FinalTerminalEofPending,
                lease_generation: generation,
                owner_guardian_incarnation: None,
                owner_mux_incarnation: None,
                owner_guardian_build_identity_digest: None,
                owner_mux_build_identity_digest: None,
                completed_successor_handoffs: self.completed_successor_handoffs,
                output_sequence: self.next_output_sequence,
                output_terminal_reason: self.output_terminal.map(|state| state.reason),
                output_child_exit_observed: self
                    .output_terminal
                    .and_then(|state| state.child_exit_observed),
            },
        }
    }

    #[must_use]
    pub fn resource_usage(&self) -> BrokerResourceUsageV1 {
        let live_guardian_leases = match self.lease {
            BrokerLeaseState::Active { .. }
            | BrokerLeaseState::Quarantined {
                attachment_may_be_open: true,
                ..
            } => 1,
            BrokerLeaseState::AwaitingSuccessor { .. }
            | BrokerLeaseState::FinalTerminalEof { .. }
            | BrokerLeaseState::FinalTerminalEofPending { .. }
            | BrokerLeaseState::Quarantined {
                attachment_may_be_open: false,
                ..
            } => 0,
        };
        BrokerResourceUsageV1 {
            broker_pty_descriptors: 1
                + u8::from(self.proxy_reader.is_some())
                + u8::from(self.proxy_writer.is_some()),
            child_handles: 1,
            live_guardian_leases,
            retained_spawn_payload_bytes: 0,
            buffered_output_bytes: self.output_buffer.len(),
            max_buffered_output_bytes: self.limits.max_buffered_output_bytes,
            fenced_attachment_tombstones: self.fenced_attachments.len(),
            max_fenced_attachment_tombstones: self.max_fenced_attachment_tombstones(),
            completed_successor_handoffs: self.completed_successor_handoffs,
        }
    }

    /// Drain one readiness chunk into the broker-owned bounded sequence store.
    ///
    /// The separately spawned broker's event loop must call this whenever the
    /// PTY is readable, including while no guardian lease is active. Filling
    /// the fixed store stops reads and intentionally backpressures the child;
    /// production continuity additionally requires the durable output journal
    /// before this in-memory foundation can be activated.
    pub fn pump_ready_output(&mut self) -> Result<BrokerOutputPumpOutcomeV1, BrokerError> {
        if let Some(state) = self.output_terminal {
            return Ok(BrokerOutputPumpOutcomeV1::TerminalDrained(
                self.output_terminal_receipt(state, false),
            ));
        }
        let remaining = self
            .limits
            .max_buffered_output_bytes
            .checked_sub(self.output_buffer.len())
            .ok_or(BrokerError::OutputBufferInvariant)?;
        if remaining == 0 {
            return Err(BrokerError::OutputBufferFull);
        }
        let max_bytes = remaining
            .min(self.limits.max_proxy_operation_bytes)
            .min(BROKER_OUTPUT_PUMP_CHUNK_BYTES);
        let max_bytes_u64 =
            u64::try_from(max_bytes).map_err(|_| BrokerError::ProxyCapacityExhausted)?;
        self.next_output_sequence
            .checked_add(max_bytes_u64)
            .ok_or(BrokerError::ProxyCapacityExhausted)?;
        let mut chunk = [0_u8; BROKER_OUTPUT_PUMP_CHUNK_BYTES];
        let read_result = self
            .proxy_reader
            .as_mut()
            .ok_or(BrokerError::OutputBufferInvariant)?
            .read(&mut chunk[..max_bytes]);
        let bytes_drained = match read_result {
            Ok(0) => {
                return Ok(
                    self.observe_output_terminal(BrokerOutputTerminalReasonV1::ZeroLengthRead)
                );
            }
            Ok(bytes_drained) => bytes_drained,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                return Err(BrokerError::ProxyWouldBlock);
            }
            Err(error) if is_pty_terminal_eio(&error) => {
                return Ok(self.observe_output_terminal(BrokerOutputTerminalReasonV1::PtyIoClosed));
            }
            Err(_) => return Err(BrokerError::ProxyReadFailed),
        };
        let bytes_drained_u64 =
            u64::try_from(bytes_drained).map_err(|_| BrokerError::ProxyCapacityExhausted)?;
        let output_sequence_start = self.next_output_sequence;
        self.output_buffer.extend(&chunk[..bytes_drained]);
        self.next_output_sequence = self
            .next_output_sequence
            .checked_add(bytes_drained_u64)
            .ok_or(BrokerError::ProxyCapacityExhausted)?;
        Ok(BrokerOutputPumpOutcomeV1::Drained(
            BrokerOutputPumpReceiptV1 {
                output_sequence_start,
                output_sequence_end: self.next_output_sequence,
                bytes_drained,
                buffered_bytes: self.output_buffer.len(),
            },
        ))
    }

    fn observe_output_terminal(
        &mut self,
        reason: BrokerOutputTerminalReasonV1,
    ) -> BrokerOutputPumpOutcomeV1 {
        drop(self.proxy_reader.take());
        let state = BrokerOutputTerminalState {
            reason,
            child_exit_observed: self.query_child_exit(),
        };
        self.output_terminal = Some(state);
        BrokerOutputPumpOutcomeV1::TerminalDrained(self.output_terminal_receipt(state, true))
    }

    fn output_terminal_receipt(
        &mut self,
        mut state: BrokerOutputTerminalState,
        newly_observed: bool,
    ) -> BrokerOutputTerminalReceiptV1 {
        if state.child_exit_observed != Some(true) {
            state.child_exit_observed = self.query_child_exit();
            self.output_terminal = Some(state);
        }
        BrokerOutputTerminalReceiptV1 {
            reason: state.reason,
            output_sequence: self.next_output_sequence,
            buffered_bytes: self.output_buffer.len(),
            child_exit_observed: state.child_exit_observed,
            newly_observed,
        }
    }

    fn query_child_exit(&mut self) -> Option<bool> {
        self.child.try_wait().map(|status| status.is_some()).ok()
    }

    /// Admit one bounded write under the current logical attachment lease.
    pub fn admit_proxy_write(
        &self,
        attachment: &BrokerPtyAttachmentV1,
        bytes: &[u8],
    ) -> Result<BrokerProxyOperationPermitV1, BrokerError> {
        self.validate_active_attachment(attachment.identity)?;
        if bytes.is_empty() || bytes.len() > self.limits.max_proxy_operation_bytes {
            return Err(BrokerError::ProxyCapacityExhausted);
        }
        Ok(BrokerProxyOperationPermitV1 {
            operation_id: Uuid::new_v4(),
            attachment: attachment.identity,
            operation: BrokerProxyOperationV1::Write {
                bytes: bytes.len(),
                digest: Sha256::digest(bytes).into(),
            },
        })
    }

    /// Admit one bounded read from the broker's acknowledged output prefix.
    ///
    /// Delivery does not release or skip bytes. Until the lease acknowledges
    /// a returned sequence prefix, every read replays from that same prefix so
    /// a lost response cannot silently advance the guardian past output.
    pub fn admit_proxy_read(
        &self,
        attachment: &BrokerPtyAttachmentV1,
        max_bytes: usize,
    ) -> Result<BrokerProxyOperationPermitV1, BrokerError> {
        self.validate_active_attachment(attachment.identity)?;
        if max_bytes == 0 || max_bytes > self.limits.max_proxy_operation_bytes {
            return Err(BrokerError::ProxyCapacityExhausted);
        }
        Ok(BrokerProxyOperationPermitV1 {
            operation_id: Uuid::new_v4(),
            attachment: attachment.identity,
            operation: BrokerProxyOperationV1::Read { max_bytes },
        })
    }

    /// Admit acknowledgement of a delivered output prefix. Only an executed
    /// acknowledgement releases bounded catch-up bytes from the broker store.
    pub fn admit_proxy_output_ack(
        &self,
        attachment: &BrokerPtyAttachmentV1,
        through_sequence: u64,
    ) -> Result<BrokerProxyOperationPermitV1, BrokerError> {
        self.validate_active_attachment(attachment.identity)?;
        if through_sequence < self.buffer_start_sequence
            || through_sequence > self.current_lease_output_cursor
        {
            return Err(BrokerError::InvalidProxyOutputAck);
        }
        Ok(BrokerProxyOperationPermitV1 {
            operation_id: Uuid::new_v4(),
            attachment: attachment.identity,
            operation: BrokerProxyOperationV1::AcknowledgeOutput { through_sequence },
        })
    }

    /// Admit one geometry change under the current logical attachment lease.
    pub fn admit_proxy_resize(
        &self,
        attachment: &BrokerPtyAttachmentV1,
        size: PtySize,
    ) -> Result<BrokerProxyOperationPermitV1, BrokerError> {
        self.validate_active_attachment(attachment.identity)?;
        if size.rows == 0 || size.cols == 0 {
            return Err(BrokerError::InvalidProxyOperation);
        }
        Ok(BrokerProxyOperationPermitV1 {
            operation_id: Uuid::new_v4(),
            attachment: attachment.identity,
            operation: BrokerProxyOperationV1::Resize { size },
        })
    }

    /// Revalidate and execute one admitted write immediately before effect.
    ///
    /// A low-level partial-write failure is classified indeterminate. The
    /// future activated broker must place this behind its durable input
    /// intent/disposition Query/Ack protocol before exposing it remotely.
    pub fn execute_proxy_write(
        &mut self,
        permit: BrokerProxyOperationPermitV1,
        bytes: &[u8],
    ) -> Result<BrokerProxyEffectReceiptV1, BrokerError> {
        self.validate_proxy_permit(&permit, BrokerProxyOperationKindV1::Write)?;
        let BrokerProxyOperationV1::Write {
            bytes: expected_bytes,
            digest,
        } = permit.operation
        else {
            return Err(BrokerError::InvalidProxyOperation);
        };
        let actual_digest: [u8; 32] = Sha256::digest(bytes).into();
        if bytes.len() != expected_bytes || actual_digest != digest {
            return Err(BrokerError::ProxyPayloadMismatch);
        }
        let writer = self
            .proxy_writer
            .as_mut()
            .ok_or(BrokerError::FinalTerminalEofAlreadySent)?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|_| BrokerError::ProxyEffectIndeterminate)?;
        Ok(BrokerProxyEffectReceiptV1 {
            operation_id: permit.operation_id,
            lease_generation: permit.attachment.lease_generation,
        })
    }

    /// Revalidate and replay once from the acknowledged output prefix.
    ///
    /// The authoritative prefix advances only in
    /// [`Self::execute_proxy_output_ack`]. A lost reply followed by a new read
    /// therefore yields the same sequence range and bytes at least once.
    pub fn execute_proxy_read(
        &mut self,
        permit: BrokerProxyOperationPermitV1,
        output: &mut [u8],
    ) -> Result<BrokerProxyReadReceiptV1, BrokerError> {
        self.validate_proxy_permit(&permit, BrokerProxyOperationKindV1::Read)?;
        let BrokerProxyOperationV1::Read { max_bytes } = permit.operation else {
            return Err(BrokerError::InvalidProxyOperation);
        };
        if output.len() < max_bytes {
            return Err(BrokerError::ProxyCapacityExhausted);
        }
        let available = self
            .next_output_sequence
            .checked_sub(self.buffer_start_sequence)
            .ok_or(BrokerError::OutputBufferInvariant)?;
        let requested =
            u64::try_from(max_bytes).map_err(|_| BrokerError::ProxyCapacityExhausted)?;
        let bytes_read_u64 = available.min(requested);
        if bytes_read_u64 == 0 {
            return Err(if self.output_terminal.is_some() {
                BrokerError::ProxyOutputTerminalDrained
            } else {
                BrokerError::ProxyWouldBlock
            });
        }
        let bytes_read =
            usize::try_from(bytes_read_u64).map_err(|_| BrokerError::ProxyCapacityExhausted)?;
        if bytes_read > self.output_buffer.len() {
            return Err(BrokerError::OutputBufferInvariant);
        }
        for (destination, source) in output[..bytes_read]
            .iter_mut()
            .zip(self.output_buffer.iter().take(bytes_read))
        {
            *destination = *source;
        }
        let output_sequence_start = self.buffer_start_sequence;
        let output_sequence_end = output_sequence_start
            .checked_add(bytes_read_u64)
            .ok_or(BrokerError::ProxyCapacityExhausted)?;
        self.current_lease_output_cursor =
            self.current_lease_output_cursor.max(output_sequence_end);
        Ok(BrokerProxyReadReceiptV1 {
            operation_id: permit.operation_id,
            lease_generation: permit.attachment.lease_generation,
            output_sequence_start,
            output_sequence_end,
            bytes_read,
        })
    }

    /// Revalidate and commit one output-prefix acknowledgement.
    pub fn execute_proxy_output_ack(
        &mut self,
        permit: BrokerProxyOperationPermitV1,
    ) -> Result<BrokerProxyEffectReceiptV1, BrokerError> {
        self.validate_proxy_permit(&permit, BrokerProxyOperationKindV1::AcknowledgeOutput)?;
        let BrokerProxyOperationV1::AcknowledgeOutput { through_sequence } = permit.operation
        else {
            return Err(BrokerError::InvalidProxyOperation);
        };
        if through_sequence < self.buffer_start_sequence
            || through_sequence > self.current_lease_output_cursor
        {
            return Err(BrokerError::InvalidProxyOutputAck);
        }
        let acknowledged_u64 = through_sequence
            .checked_sub(self.buffer_start_sequence)
            .ok_or(BrokerError::InvalidProxyOutputAck)?;
        let acknowledged =
            usize::try_from(acknowledged_u64).map_err(|_| BrokerError::InvalidProxyOutputAck)?;
        if acknowledged > self.output_buffer.len() {
            return Err(BrokerError::OutputBufferInvariant);
        }
        self.output_buffer.drain(..acknowledged);
        self.buffer_start_sequence = through_sequence;
        Ok(BrokerProxyEffectReceiptV1 {
            operation_id: permit.operation_id,
            lease_generation: permit.attachment.lease_generation,
        })
    }

    /// Revalidate and execute one admitted resize immediately before effect.
    pub fn execute_proxy_resize(
        &mut self,
        permit: BrokerProxyOperationPermitV1,
    ) -> Result<BrokerProxyEffectReceiptV1, BrokerError> {
        self.validate_proxy_permit(&permit, BrokerProxyOperationKindV1::Resize)?;
        let BrokerProxyOperationV1::Resize { size } = permit.operation else {
            return Err(BrokerError::InvalidProxyOperation);
        };
        self.broker_master
            .resize(size)
            .map_err(|_| BrokerError::ProxyEffectIndeterminate)?;
        Ok(BrokerProxyEffectReceiptV1 {
            operation_id: permit.operation_id,
            lease_generation: permit.attachment.lease_generation,
        })
    }

    fn validate_active_attachment(
        &self,
        attachment: BrokerAttachmentIdentityV1,
    ) -> Result<(), BrokerError> {
        match self.lease {
            BrokerLeaseState::Active {
                attachment: active, ..
            } if active == attachment => Ok(()),
            _ => Err(BrokerError::StaleProxyLease),
        }
    }

    fn validate_proxy_permit(
        &self,
        permit: &BrokerProxyOperationPermitV1,
        expected: BrokerProxyOperationKindV1,
    ) -> Result<(), BrokerError> {
        self.validate_active_attachment(permit.attachment)?;
        if permit.operation_id.is_nil() || permit.operation.kind() != expected {
            return Err(BrokerError::InvalidProxyOperation);
        }
        Ok(())
    }

    /// Fence the active attachment after authenticated connection EOF.
    pub fn observe_authenticated_control_eof(
        &mut self,
        eof: BrokerAuthenticatedControlEofV1,
    ) -> Result<BrokerControlEofOutcomeV1, BrokerError> {
        if self.fenced_attachments.contains(&eof.attachment) {
            return Ok(BrokerControlEofOutcomeV1::AlreadyObserved);
        }
        match self.lease {
            BrokerLeaseState::Active { attachment, .. } if attachment == eof.attachment => {
                let Some(next_generation) = attachment.lease_generation.checked_add(1) else {
                    self.quarantine(BrokerQuarantineReasonV1::LeaseGenerationExhausted, false);
                    return Err(BrokerError::Quarantined);
                };
                if self.fenced_attachments.len() >= self.max_fenced_attachment_tombstones() {
                    self.quarantine(
                        BrokerQuarantineReasonV1::FencedAttachmentCapacityExhausted,
                        false,
                    );
                    return Err(BrokerError::FencedAttachmentCapacityExhausted);
                }
                self.fenced_attachments.push_back(attachment);
                self.lease = BrokerLeaseState::AwaitingSuccessor {
                    predecessor: attachment,
                    next_generation,
                };
                Ok(BrokerControlEofOutcomeV1::AwaitingSuccessor { next_generation })
            }
            BrokerLeaseState::AwaitingSuccessor { predecessor, .. }
                if predecessor == eof.attachment =>
            {
                Ok(BrokerControlEofOutcomeV1::AlreadyObserved)
            }
            BrokerLeaseState::Quarantined { .. } => Err(BrokerError::Quarantined),
            BrokerLeaseState::FinalTerminalEof { .. } => {
                Err(BrokerError::FinalTerminalEofAlreadySent)
            }
            BrokerLeaseState::FinalTerminalEofPending { .. } => {
                Err(BrokerError::FinalTerminalEofInProgress)
            }
            BrokerLeaseState::Active { .. } | BrokerLeaseState::AwaitingSuccessor { .. } => {
                let attachment_may_be_open = matches!(self.lease, BrokerLeaseState::Active { .. });
                self.quarantine(
                    BrokerQuarantineReasonV1::ConflictingControlEof,
                    attachment_may_be_open,
                );
                Err(BrokerError::ConflictingControlEof)
            }
        }
    }

    /// Rotate the one active attachment after exact successor handoff proof.
    pub fn attach_successor(
        &mut self,
        authority: BrokerSuccessorHandoffAuthorityV1,
    ) -> Result<BrokerSuccessorAttachOutcomeV1, BrokerError> {
        if matches!(
            self.lease,
            BrokerLeaseState::FinalTerminalEof { .. }
                | BrokerLeaseState::FinalTerminalEofPending { .. }
        ) {
            return Err(
                if matches!(self.lease, BrokerLeaseState::FinalTerminalEof { .. }) {
                    BrokerError::FinalTerminalEofAlreadySent
                } else {
                    BrokerError::FinalTerminalEofInProgress
                },
            );
        }
        if authority.broker_incarnation != self.broker_incarnation
            || authority.durable_pane_id != self.binding.durable_pane_id
            || authority.spawn_effect_id != self.binding.spawn_effect_id
            || authority.handoff_id.is_nil()
            || !authority.successor.is_valid()
        {
            let attachment_may_be_open = matches!(self.lease, BrokerLeaseState::Active { .. });
            self.quarantine(
                BrokerQuarantineReasonV1::ConflictingSuccessorHandoff,
                attachment_may_be_open,
            );
            return Err(BrokerError::ConflictingSuccessorHandoff);
        }

        match self.lease {
            BrokerLeaseState::Active {
                attachment,
                last_handoff_id: Some(last_handoff_id),
            } if last_handoff_id == authority.handoff_id
                && attachment.owner == authority.successor
                && attachment.lease_generation
                    == authority.predecessor.lease_generation.saturating_add(1) =>
            {
                Ok(BrokerSuccessorAttachOutcomeV1::RecoveredExistingLease {
                    attachment: BrokerPtyAttachmentV1 {
                        identity: attachment,
                    },
                    status: self.status(),
                })
            }
            BrokerLeaseState::AwaitingSuccessor {
                predecessor,
                next_generation,
            } => {
                if predecessor != authority.predecessor
                    || authority.successor.guardian_incarnation
                        == predecessor.owner.guardian_incarnation
                    || authority.successor.connection_id == predecessor.owner.connection_id
                {
                    self.quarantine(BrokerQuarantineReasonV1::ConflictingSuccessorHandoff, false);
                    return Err(BrokerError::ConflictingSuccessorHandoff);
                }
                if self.completed_successor_handoffs >= self.limits.max_successor_handoffs {
                    self.quarantine(BrokerQuarantineReasonV1::HandoffCapacityExhausted, false);
                    return Err(BrokerError::HandoffCapacityExhausted);
                }
                let attachment = BrokerAttachmentIdentityV1 {
                    broker_incarnation: self.broker_incarnation,
                    durable_pane_id: self.binding.durable_pane_id,
                    spawn_effect_id: self.binding.spawn_effect_id,
                    attachment_id: Uuid::new_v4(),
                    owner: authority.successor,
                    lease_generation: next_generation,
                };
                self.completed_successor_handoffs = self
                    .completed_successor_handoffs
                    .checked_add(1)
                    .ok_or(BrokerError::HandoffCapacityExhausted)?;
                self.lease = BrokerLeaseState::Active {
                    attachment,
                    last_handoff_id: Some(authority.handoff_id),
                };
                self.current_lease_output_cursor = self.buffer_start_sequence;
                Ok(BrokerSuccessorAttachOutcomeV1::Attached(
                    BrokerPtyAttachmentV1 {
                        identity: attachment,
                    },
                ))
            }
            BrokerLeaseState::Quarantined { .. } => Err(BrokerError::Quarantined),
            BrokerLeaseState::FinalTerminalEof { .. } => {
                Err(BrokerError::FinalTerminalEofAlreadySent)
            }
            BrokerLeaseState::FinalTerminalEofPending { .. } => {
                Err(BrokerError::FinalTerminalEofInProgress)
            }
            BrokerLeaseState::Active { .. } => {
                self.quarantine(BrokerQuarantineReasonV1::ConflictingSuccessorHandoff, true);
                Err(BrokerError::ConflictingSuccessorHandoff)
            }
        }
    }

    /// Deliver terminal EOF only after the last attachment was fenced closed.
    ///
    /// The required authority intentionally has no production constructor;
    /// the later durable retirement protocol must mint it. Attachment drops
    /// themselves are always byte-silent.
    pub fn send_final_terminal_eof(
        &mut self,
        authority: BrokerFinalTerminalEofAuthorityV1,
    ) -> Result<(), BrokerError> {
        let (predecessor, generation, begin_retirement) = match self.lease {
            BrokerLeaseState::AwaitingSuccessor {
                predecessor,
                next_generation,
            } => (predecessor, next_generation, true),
            BrokerLeaseState::FinalTerminalEofPending {
                predecessor,
                generation,
            } => (predecessor, generation, false),
            BrokerLeaseState::FinalTerminalEof { .. } => {
                return Err(BrokerError::FinalTerminalEofAlreadySent);
            }
            BrokerLeaseState::Active { .. } | BrokerLeaseState::Quarantined { .. } => {
                return Err(BrokerError::FinalTerminalEofNotFenced);
            }
        };
        if authority.broker_incarnation != self.broker_incarnation
            || authority.durable_pane_id != self.binding.durable_pane_id
            || authority.spawn_effect_id != self.binding.spawn_effect_id
            || authority.child_nonce != self.child_identity.broker_child_nonce
            || authority.predecessor != predecessor
        {
            return Err(BrokerError::FinalTerminalEofAuthorityMismatch);
        }
        if begin_retirement {
            let proxy_writer = self
                .proxy_writer
                .take()
                .ok_or(BrokerError::FinalTerminalEofInProgress)?;
            drop(proxy_writer);
            self.lease = BrokerLeaseState::FinalTerminalEofPending {
                predecessor,
                generation,
            };
        }
        if self.broker_master.send_terminal_eof().is_err() {
            return Err(BrokerError::FinalTerminalEofFailed);
        }
        self.lease = BrokerLeaseState::FinalTerminalEof {
            predecessor,
            generation,
        };
        Ok(())
    }

    fn quarantine(&mut self, reason: BrokerQuarantineReasonV1, attachment_may_be_open: bool) {
        self.lease = BrokerLeaseState::Quarantined {
            reason,
            attachment_may_be_open,
        };
    }

    fn max_fenced_attachment_tombstones(&self) -> usize {
        usize::try_from(self.limits.max_successor_handoffs)
            .expect("validated successor handoff limit fits usize")
            + 1
    }

    const fn current_generation(&self) -> u64 {
        match self.lease {
            BrokerLeaseState::Active { attachment, .. } => attachment.lease_generation,
            BrokerLeaseState::AwaitingSuccessor {
                next_generation, ..
            } => next_generation,
            BrokerLeaseState::Quarantined { .. } => self.completed_successor_handoffs as u64,
            BrokerLeaseState::FinalTerminalEof { generation, .. } => generation,
            BrokerLeaseState::FinalTerminalEofPending { generation, .. } => generation,
        }
    }

    #[cfg(test)]
    fn terminate_and_wait_for_test(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    #[cfg(test)]
    fn wait_for_test(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    #[cfg(test)]
    fn child_is_running_for_test(&mut self) -> std::io::Result<bool> {
        self.child.try_wait().map(|status| status.is_none())
    }
}

/// Logical authority for one guardian to use the broker's bounded I/O proxy.
///
/// This value contains no PTY descriptor and cannot be cloned or serialized.
/// A successor receives a new identity/generation while the broker retains the
/// same sole reader, writer, master, child, and output cursor.
pub struct BrokerPtyAttachmentV1 {
    identity: BrokerAttachmentIdentityV1,
}

impl BrokerPtyAttachmentV1 {
    #[must_use]
    pub const fn identity(&self) -> BrokerAttachmentIdentityV1 {
        self.identity
    }
}

/// Nonduplicable evidence that the transport observed authenticated EOF for
/// one exact attachment.  Raw UUIDs cannot construct this value.
pub struct BrokerAuthenticatedControlEofV1 {
    attachment: BrokerAttachmentIdentityV1,
}

impl BrokerAuthenticatedControlEofV1 {
    /// Future control-transport integration seam. Call only after the broker
    /// has closed the authenticated proxy connection and removed all queued
    /// admissions for it. Since no PTY descriptor is transferred, rotating
    /// the broker-side token revokes the predecessor's ability to perform new
    /// effects; every previously queued permit is revalidated before effect.
    pub(crate) const fn from_authenticated_transport_close(
        attachment: BrokerAttachmentIdentityV1,
    ) -> Self {
        Self { attachment }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerControlEofOutcomeV1 {
    AwaitingSuccessor { next_generation: u64 },
    AlreadyObserved,
}

/// Reserved nonduplicable successor authority.
///
/// There is intentionally no production constructor.  A later broker control
/// protocol must mint this only from a durable, authenticated handoff record;
/// until then the successor path is impossible to activate in production.
pub struct BrokerSuccessorHandoffAuthorityV1 {
    broker_incarnation: Uuid,
    durable_pane_id: Uuid,
    spawn_effect_id: Uuid,
    handoff_id: Uuid,
    predecessor: BrokerAttachmentIdentityV1,
    successor: BrokerGuardianOwnerIdentity,
}

/// Nonduplicable terminal-retirement authority reserved for the future
/// durable broker control protocol. There is no production constructor.
pub struct BrokerFinalTerminalEofAuthorityV1 {
    broker_incarnation: Uuid,
    durable_pane_id: Uuid,
    spawn_effect_id: Uuid,
    child_nonce: Uuid,
    predecessor: BrokerAttachmentIdentityV1,
}

impl std::fmt::Debug for BrokerFinalTerminalEofAuthorityV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerFinalTerminalEofAuthorityV1")
            .field("broker_incarnation", &self.broker_incarnation)
            .field("durable_pane_id", &self.durable_pane_id)
            .field("spawn_effect_id", &self.spawn_effect_id)
            .field("child_nonce", &"[REDACTED]")
            .field("predecessor", &self.predecessor)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BrokerSuccessorHandoffAuthorityV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrokerSuccessorHandoffAuthorityV1")
            .field("broker_incarnation", &self.broker_incarnation)
            .field("durable_pane_id", &self.durable_pane_id)
            .field("spawn_effect_id", &self.spawn_effect_id)
            .field("handoff_id", &self.handoff_id)
            .field("predecessor", &self.predecessor)
            .field("successor", &"[AUTHENTICATED]")
            .finish_non_exhaustive()
    }
}

pub enum BrokerSuccessorAttachOutcomeV1 {
    Attached(BrokerPtyAttachmentV1),
    RecoveredExistingLease {
        attachment: BrokerPtyAttachmentV1,
        status: BrokerPaneStatusV1,
    },
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BrokerError {
    #[error("broker resource capacity is invalid or exhausted")]
    CapacityExhausted,
    #[error("broker authenticated connection authority is invalid")]
    InvalidAuthenticatedAuthority,
    #[error("broker authenticated connection does not match the Genesis reservation")]
    AuthenticatedAuthorityMismatch,
    #[error("broker Genesis reservation is invalid")]
    InvalidGenesisReservation,
    #[error("broker Spawn payload is invalid")]
    InvalidSpawnPayload,
    #[error("canonical Spawn payload does not match its Genesis reservation")]
    SpawnPayloadBindingMismatch,
    #[error("broker could not allocate the PTY")]
    PtyAllocationFailed,
    #[error("broker could not prepare its bounded PTY I/O proxy")]
    ProxyIoPreparationFailed,
    #[error("broker control lease does not match the prepared pane")]
    ControlLeaseMismatch,
    #[error("durable Genesis pre-Spawn intent does not match the prepared pane")]
    DurablePreSpawnIntentMismatch,
    #[error("durable Genesis catalog checksum is invalid")]
    InvalidCatalogChecksum,
    #[error("broker could not spawn the child")]
    ChildSpawnFailed,
    #[error("broker child did not expose an exact process identity")]
    ChildIdentityUnavailable,
    #[error("conflicting authenticated control EOF quarantined the pane")]
    ConflictingControlEof,
    #[error("conflicting successor handoff quarantined the pane")]
    ConflictingSuccessorHandoff,
    #[error("successor handoff capacity is exhausted")]
    HandoffCapacityExhausted,
    #[error("fenced attachment tombstone capacity is exhausted")]
    FencedAttachmentCapacityExhausted,
    #[error("broker pane is quarantined")]
    Quarantined,
    #[error("broker proxy operation exceeds its fixed resource bounds")]
    ProxyCapacityExhausted,
    #[error("broker proxy lease is stale or no longer active")]
    StaleProxyLease,
    #[error("broker proxy operation authority is invalid")]
    InvalidProxyOperation,
    #[error("broker proxy write bytes do not match their admitted commitment")]
    ProxyPayloadMismatch,
    #[error("broker proxy effect may be partially applied")]
    ProxyEffectIndeterminate,
    #[error("broker proxy read has no bytes ready")]
    ProxyWouldBlock,
    #[error("broker PTY output is terminal and every retained byte was acknowledged")]
    ProxyOutputTerminalDrained,
    #[error("broker proxy read failed")]
    ProxyReadFailed,
    #[error("broker output buffer is full; child backpressure is active")]
    OutputBufferFull,
    #[error("broker output buffer sequence invariant failed")]
    OutputBufferInvariant,
    #[error("requested proxy output is older than the retained sequence window")]
    ProxyOutputGap,
    #[error("proxy output acknowledgement is outside the delivered prefix")]
    InvalidProxyOutputAck,
    #[error("final terminal EOF requires a fenced, closed attachment")]
    FinalTerminalEofNotFenced,
    #[error("final terminal EOF authority does not match the retained child")]
    FinalTerminalEofAuthorityMismatch,
    #[error("final terminal EOF was already sent")]
    FinalTerminalEofAlreadySent,
    #[error("final terminal EOF retirement is pending reconciliation")]
    FinalTerminalEofInProgress,
    #[error("final terminal EOF delivery failed; its effect may be indeterminate")]
    FinalTerminalEofFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn sealed(byte: u8) -> SealedAtomicBuildIdentity {
        SealedAtomicBuildIdentity::from_lower_hex(&format!("{byte:02x}").repeat(32))
            .expect("sealed test identity")
    }

    fn authority(
        broker_incarnation: Uuid,
        guardian_incarnation: Uuid,
        connection_id: Uuid,
        mux_incarnation: Uuid,
        mux_build_byte: u8,
        guardian_build_byte: u8,
    ) -> BrokerAuthenticatedGuardianConnectionV1 {
        BrokerAuthenticatedGuardianConnectionV1::from_authenticated_transport(
            broker_incarnation,
            guardian_incarnation,
            connection_id,
            mux_incarnation,
            sealed(mux_build_byte),
            sealed(guardian_build_byte),
        )
        .expect("authenticated test authority")
    }

    fn binding_for(
        payload: &GuardianSpawnPayload,
        authority: &BrokerAuthenticatedGuardianConnectionV1,
    ) -> BrokerGenesisBinding {
        let encoded = payload.encode().expect("canonical Spawn payload");
        BrokerGenesisBinding {
            mux_incarnation: authority.owner.mux_incarnation,
            spawn_effect_id: id(10),
            durable_pane_id: id(11),
            origin_request_id: id(12),
            spawn_payload_bytes: u64::try_from(encoded.len()).expect("payload length"),
            spawn_payload_digest: Sha256::digest(encoded.as_slice()).into(),
            spawning_mux_build_identity_digest: authority.owner.mux_build_identity_digest,
            live_guardian_build_identity_digest: authority.owner.guardian_build_identity_digest,
            rows: payload.size().rows,
            cols: payload.size().cols,
            pixel_width: payload.size().pixel_width,
            pixel_height: payload.size().pixel_height,
            checkpoint_identity_digest: [0x31; 32],
            boundary_identity_digest: [0x32; 32],
            upload_id: id(13),
        }
    }

    fn command_payload(script: &str, sentinel: &std::path::Path) -> GuardianSpawnPayload {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(script);
        command.env("BROKER_SENTINEL", sentinel);
        GuardianSpawnPayload::new(
            command,
            PtySize {
                rows: 31,
                cols: 101,
                pixel_width: 8,
                pixel_height: 16,
            },
        )
        .expect("valid test Spawn")
    }

    fn prepare_for_test(
        payload: GuardianSpawnPayload,
        binding: BrokerGenesisBinding,
        authority: &BrokerAuthenticatedGuardianConnectionV1,
        limits: BrokerResourceLimitsV1,
    ) -> Result<(BrokerPreparedPaneV1, BrokerControlLeaseV1), BrokerError> {
        BrokerPreparedPaneV1::prepare_binding(binding, payload, authority, limits)
    }

    fn commit_for_test(
        prepared: BrokerPreparedPaneV1,
        control: BrokerControlLeaseV1,
        binding: BrokerGenesisBinding,
    ) -> Result<BrokerAdoptionV1, BrokerError> {
        let proof = BrokerDurablePreSpawnIntentProof {
            binding,
            catalog_candidate_checksum: [0x71; BROKER_CATALOG_CHECKSUM_BYTES],
        };
        prepared.commit_with_proof(control, &proof)
    }

    fn wal_authenticator(byte: u8) -> GuardianBrokerSpawnWalAuthenticatorV1 {
        mux::guardian_protocol::GuardianSecret::from_bytes([byte; 32])
            .expect("nonzero guardian test secret")
            .broker_spawn_wal_authenticator()
            .expect("derive broker WAL authenticator")
    }

    fn control_authenticator(byte: u8) -> GuardianBrokerControlAuthenticatorV1 {
        mux::guardian_protocol::GuardianSecret::from_bytes([byte; 32])
            .expect("nonzero guardian test secret")
            .broker_control_authenticator()
            .expect("derive broker control authenticator")
    }

    fn wal_identity(binding: BrokerGenesisBinding) -> BrokerSpawnWalIdentityV1 {
        BrokerSpawnWalIdentityV1::from_binding(id(900), id(901), binding)
            .expect("valid broker WAL identity")
    }

    #[test]
    fn control_codec_is_bounded_direction_separated_and_mutation_sensitive() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<BrokerControlWireFrameV1>();

        let authority = control_authenticator(0x5a);
        let request = BrokerControlRequestV1::new(
            BrokerControlRequestHeaderV1 {
                operation: BrokerControlOperationV1::Hello,
                request_id: id(701),
                broker_incarnation: Uuid::nil(),
                guardian_incarnation: id(702),
                connection_id: id(703),
                mux_incarnation: id(704),
                guardian_build_identity_digest: [0x71; 32],
                mux_build_identity_digest: [0x72; 32],
                durable_pane_id: Uuid::nil(),
                lease_generation: 0,
                operation_id: Uuid::nil(),
            },
            &[],
        )
        .expect("canonical Hello request");
        let encoded = encode_broker_control_request(&authority, &request).expect("encode request");
        assert_eq!(encoded.as_slice().len(), BROKER_CONTROL_REQUEST_FIXED_BYTES);
        let decoded =
            decode_broker_control_request(&authority, encoded.as_slice()).expect("decode request");
        assert_eq!(decoded.header, request.header);
        assert_eq!(decoded.payload(), &[] as &[u8]);

        let mut mutated = encoded.as_slice().to_vec();
        mutated[20] ^= 1;
        assert!(matches!(
            decode_broker_control_request(&authority, &mutated),
            Err(BrokerControlProtocolError::AuthenticationFailed)
        ));
        let mut length_mutated = encoded.as_slice().to_vec();
        length_mutated[3] ^= 1;
        assert!(matches!(
            decode_broker_control_request(&authority, &length_mutated),
            Err(BrokerControlProtocolError::InvalidLength)
        ));

        let response = BrokerControlResponseV1::new(
            BrokerControlResponseHeaderV1 {
                operation: BrokerControlOperationV1::Hello,
                status: BrokerControlResponseStatusV1::Applied,
                request_id: request.header.request_id,
                broker_incarnation: id(705),
                guardian_incarnation: request.header.guardian_incarnation,
                connection_id: request.header.connection_id,
                durable_pane_id: Uuid::nil(),
                lease_generation: 0,
                operation_id: Uuid::nil(),
                child_identity: None,
                output_sequence_start: 0,
                output_sequence_end: 0,
            },
            &[],
        )
        .expect("canonical Hello response");
        let encoded_response =
            encode_broker_control_response(&authority, &response).expect("encode response");
        assert_eq!(
            encoded_response.as_slice().len(),
            BROKER_CONTROL_RESPONSE_FIXED_BYTES
        );
        let decoded_response =
            decode_broker_control_response(&authority, encoded_response.as_slice())
                .expect("decode response");
        assert_eq!(decoded_response.header, response.header);
        assert_eq!(decoded_response.payload(), &[] as &[u8]);
        assert!(
            decode_broker_control_request(&authority, encoded_response.as_slice()).is_err(),
            "a response-direction frame was accepted as a request"
        );
        assert!(
            decode_broker_control_response(&authority, encoded.as_slice()).is_err(),
            "a request-direction frame was accepted as a response"
        );

        let invalid_write = BrokerControlRequestV1::new(
            BrokerControlRequestHeaderV1 {
                operation: BrokerControlOperationV1::Write,
                request_id: id(706),
                broker_incarnation: id(705),
                guardian_incarnation: id(702),
                connection_id: id(703),
                mux_incarnation: id(704),
                guardian_build_identity_digest: [0x71; 32],
                mux_build_identity_digest: [0x72; 32],
                durable_pane_id: id(707),
                lease_generation: 1,
                operation_id: id(708),
            },
            &[],
        );
        assert!(matches!(
            invalid_write,
            Err(BrokerControlProtocolError::InvalidShape)
        ));

        let leased_operations: [(BrokerControlOperationV1, &[u8]); 6] = [
            (BrokerControlOperationV1::Write, b"must-be-fenced"),
            (BrokerControlOperationV1::Resize, &[0; 8]),
            (BrokerControlOperationV1::ReadOutput, &[0; 4]),
            (BrokerControlOperationV1::AcknowledgeOutput, &[0; 8]),
            (BrokerControlOperationV1::AttachSuccessor, &[]),
            (BrokerControlOperationV1::ClosePane, &[]),
        ];
        for (operation, payload) in leased_operations {
            let zero_generation_request = BrokerControlRequestV1::new(
                BrokerControlRequestHeaderV1 {
                    operation,
                    request_id: id(709),
                    broker_incarnation: id(705),
                    guardian_incarnation: id(702),
                    connection_id: id(703),
                    mux_incarnation: id(704),
                    guardian_build_identity_digest: [0x71; 32],
                    mux_build_identity_digest: [0x72; 32],
                    durable_pane_id: id(707),
                    lease_generation: 0,
                    operation_id: id(710),
                },
                payload,
            );
            assert!(
                matches!(
                    zero_generation_request,
                    Err(BrokerControlProtocolError::InvalidShape)
                ),
                "{operation:?} accepted reserved generation zero"
            );
        }

        for operation in [
            BrokerControlOperationV1::QueryEffect,
            BrokerControlOperationV1::AcknowledgeEffect,
        ] {
            BrokerControlRequestV1::new(
                BrokerControlRequestHeaderV1 {
                    operation,
                    request_id: id(709),
                    broker_incarnation: id(705),
                    guardian_incarnation: id(702),
                    connection_id: id(703),
                    mux_incarnation: id(704),
                    guardian_build_identity_digest: [0x71; 32],
                    mux_build_identity_digest: [0x72; 32],
                    durable_pane_id: id(707),
                    lease_generation: 0,
                    operation_id: id(710),
                },
                &[],
            )
            .expect("generation zero is reserved for the pre-lease Spawn receipt");
        }

        let child_identity = BrokerKernelChildIdentityV1 {
            process_id: 711,
            broker_child_nonce: id(712),
            kernel_start_identity_digest: [0x73; 32],
        };
        let forged_rejected_write = BrokerControlResponseV1::new(
            BrokerControlResponseHeaderV1 {
                operation: BrokerControlOperationV1::Write,
                status: BrokerControlResponseStatusV1::Rejected,
                request_id: id(713),
                broker_incarnation: id(705),
                guardian_incarnation: id(702),
                connection_id: id(703),
                durable_pane_id: id(707),
                lease_generation: 1,
                operation_id: id(714),
                child_identity: Some(child_identity),
                output_sequence_start: 0,
                output_sequence_end: 0,
            },
            &[],
        );
        assert!(matches!(
            forged_rejected_write,
            Err(BrokerControlProtocolError::InvalidShape)
        ));

        let mismatched_read_range = BrokerControlResponseV1::new(
            BrokerControlResponseHeaderV1 {
                operation: BrokerControlOperationV1::ReadOutput,
                status: BrokerControlResponseStatusV1::Applied,
                request_id: id(715),
                broker_incarnation: id(705),
                guardian_incarnation: id(702),
                connection_id: id(703),
                durable_pane_id: id(707),
                lease_generation: 1,
                operation_id: id(716),
                child_identity: None,
                output_sequence_start: 40,
                output_sequence_end: 44,
            },
            b"five!",
        );
        assert!(matches!(
            mismatched_read_range,
            Err(BrokerControlProtocolError::InvalidShape)
        ));

        let spawn_query = BrokerControlResponseV1::new(
            BrokerControlResponseHeaderV1 {
                operation: BrokerControlOperationV1::QueryEffect,
                status: BrokerControlResponseStatusV1::Recovered,
                request_id: id(717),
                broker_incarnation: id(705),
                guardian_incarnation: id(702),
                connection_id: id(703),
                durable_pane_id: id(707),
                lease_generation: 0,
                operation_id: id(718),
                child_identity: Some(child_identity),
                output_sequence_start: 0,
                output_sequence_end: 0,
            },
            &[],
        )
        .expect("generation-zero QueryEffect may recover only Spawn identity");
        assert_eq!(spawn_query.header.child_identity, Some(child_identity));

        let forged_leased_query = BrokerControlResponseV1::new(
            BrokerControlResponseHeaderV1 {
                lease_generation: 1,
                ..spawn_query.header
            },
            &[],
        );
        assert!(matches!(
            forged_leased_query,
            Err(BrokerControlProtocolError::InvalidShape)
        ));
    }

    #[test]
    fn spawn_control_payload_round_trips_exact_admission_binding() {
        let authenticated = authority(id(721), id(722), id(723), id(724), 0x81, 0x82);
        let sentinel = crate::canonical_test_temp_root().join("broker-control-spawn-never-created");
        let payload = command_payload("printf never-spawned", &sentinel);
        let binding = binding_for(&payload, &authenticated);
        let request = BrokerSpawnControlRequestV1::from_parts(
            id(725),
            id(726),
            [0x83; BROKER_CATALOG_CHECKSUM_BYTES],
            binding,
            payload,
        )
        .expect("canonical Spawn control request");
        let encoded = request.encode().expect("encode Spawn control request");
        let decoded =
            BrokerSpawnControlRequestV1::decode(&encoded).expect("decode Spawn control request");
        assert_eq!(decoded.journal_id, id(725));
        assert_eq!(decoded.attempt_id, id(726));
        assert_eq!(decoded.binding, binding);
        assert_eq!(decoded.payload, request.payload);

        let header = BrokerControlRequestHeaderV1 {
            operation: BrokerControlOperationV1::Spawn,
            request_id: id(727),
            broker_incarnation: authenticated.broker_incarnation,
            guardian_incarnation: authenticated.owner.guardian_incarnation,
            connection_id: authenticated.owner.connection_id,
            mux_incarnation: authenticated.owner.mux_incarnation,
            guardian_build_identity_digest: authenticated.owner.guardian_build_identity_digest,
            mux_build_identity_digest: authenticated.owner.mux_build_identity_digest,
            durable_pane_id: binding.durable_pane_id,
            lease_generation: 0,
            operation_id: binding.spawn_effect_id,
        };
        decoded
            .validate_control_header(header)
            .expect("control header binds exact admission");
        let mut wrong_header = header;
        wrong_header.operation_id = id(728);
        assert!(matches!(
            decoded.validate_control_header(wrong_header),
            Err(BrokerControlProtocolError::InvalidIdentity)
        ));

        let mut mutated = encoded.to_vec();
        mutated[72 + 72] ^= 1;
        assert!(BrokerSpawnControlRequestV1::decode(&mutated).is_err());
        assert!(!sentinel.exists(), "control decode launched a user child");
    }

    fn private_catalog_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in(crate::canonical_test_temp_root())
            .expect("create private broker catalog test directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("set broker catalog test directory owner-only");
        directory
    }

    fn open_new_test_file(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create exclusive test file")
    }

    fn open_existing_test_file(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open existing test file")
    }

    fn create_test_spawn_journal(
        directory: &std::path::Path,
        identity: BrokerSpawnWalIdentityV1,
        authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> (BrokerSpawnJournalV1, std::path::PathBuf, std::path::PathBuf) {
        let wal_path = directory.join("spawn.wal");
        let head_path = directory.join("spawn.head");
        let wal = open_new_test_file(&wal_path);
        let head = open_new_test_file(&head_path);
        let mut journal = BrokerSpawnJournalV1::create(wal, head, identity, authenticator)
            .expect("create broker Spawn WAL");
        let parent = File::open(directory).expect("open test parent directory");
        journal
            .sync_parent_directory_and_activate(&parent)
            .expect("activate broker Spawn WAL directory entries");
        (journal, wal_path, head_path)
    }

    fn reopen_test_spawn_journal(
        wal_path: &std::path::Path,
        head_path: &std::path::Path,
        identity: BrokerSpawnWalIdentityV1,
        authenticator: GuardianBrokerSpawnWalAuthenticatorV1,
    ) -> BrokerSpawnJournalV1 {
        BrokerSpawnJournalV1::open(
            open_existing_test_file(wal_path),
            open_existing_test_file(head_path),
            identity,
            authenticator,
        )
        .expect("reopen authenticated broker Spawn WAL")
    }

    fn revalidate_recovered_test_journal(
        journal: &BrokerSpawnJournalV1,
    ) -> BrokerSpawnWalFilesystemRevalidationV1 {
        BrokerSpawnWalFilesystemRevalidationV1::from_revalidated_filesystem(
            journal.identity(),
            journal.wal.metadata().expect("WAL metadata").len(),
            journal.head.metadata().expect("head metadata").len(),
        )
        .expect("test filesystem revalidation")
    }

    fn test_kernel_child(process_id: u32) -> BrokerKernelChildIdentityV1 {
        BrokerKernelChildIdentityV1 {
            process_id,
            broker_child_nonce: id(u128::from(process_id) + 1_000),
            kernel_start_identity_digest: [u8::try_from(process_id % 251 + 1)
                .expect("bounded digest byte"); 32],
        }
    }

    fn proxy_write(
        pane: &mut BrokerAdoptedPaneV1,
        attachment: &BrokerPtyAttachmentV1,
        bytes: &[u8],
    ) {
        let permit = pane
            .admit_proxy_write(attachment, bytes)
            .expect("admit proxy write");
        pane.execute_proxy_write(permit, bytes)
            .expect("execute proxy write");
    }

    fn read_until(
        pane: &mut BrokerAdoptedPaneV1,
        attachment: &BrokerPtyAttachmentV1,
        needle: &[u8],
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut chunk = [0_u8; 512];
        while Instant::now() < deadline {
            let max_bytes = pane.limits.max_proxy_operation_bytes.min(chunk.len());
            let permit = pane
                .admit_proxy_read(attachment, max_bytes)
                .expect("admit proxy read");
            match pane.execute_proxy_read(permit, &mut chunk) {
                Ok(receipt) if receipt.bytes_read == 0 => break,
                Ok(receipt) => {
                    let count = receipt.bytes_read;
                    output.extend_from_slice(&chunk[..count]);
                    let ack = pane
                        .admit_proxy_output_ack(attachment, receipt.output_sequence_end)
                        .expect("admit output acknowledgement");
                    pane.execute_proxy_output_ack(ack)
                        .expect("commit output acknowledgement");
                    if output.windows(needle.len()).any(|window| window == needle) {
                        return output;
                    }
                }
                Err(BrokerError::ProxyWouldBlock) => match pane.pump_ready_output() {
                    Ok(BrokerOutputPumpOutcomeV1::Drained(receipt))
                        if receipt.bytes_drained > 0 => {}
                    Ok(BrokerOutputPumpOutcomeV1::Drained(_))
                    | Err(BrokerError::ProxyWouldBlock) => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(BrokerOutputPumpOutcomeV1::TerminalDrained(receipt)) => panic!(
                        "PTY output became terminal before {:?}: {:?}",
                        String::from_utf8_lossy(needle),
                        receipt.reason
                    ),
                    Err(error) => panic!("PTY output pump failed: {error}"),
                },
                Err(error) => panic!("PTY proxy read failed: {error}"),
            }
        }
        panic!(
            "timed out waiting for {:?}; output was {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&output)
        );
    }

    fn pump_until_sequence_advances(pane: &mut BrokerAdoptedPaneV1, baseline: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match pane.pump_ready_output() {
                Ok(BrokerOutputPumpOutcomeV1::Drained(_))
                    if pane.status().output_sequence > baseline =>
                {
                    return;
                }
                Ok(BrokerOutputPumpOutcomeV1::Drained(_)) | Err(BrokerError::ProxyWouldBlock) => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(BrokerOutputPumpOutcomeV1::TerminalDrained(receipt)) => {
                    panic!("PTY output became terminal before sequence advanced: {receipt:?}")
                }
                Err(error) => panic!("PTY output pump failed: {error}"),
            }
        }
        panic!("timed out draining output while guardian lease was absent");
    }

    fn pump_until_drained(pane: &mut BrokerAdoptedPaneV1) -> BrokerOutputPumpReceiptV1 {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match pane.pump_ready_output() {
                Ok(BrokerOutputPumpOutcomeV1::Drained(receipt)) => return receipt,
                Ok(BrokerOutputPumpOutcomeV1::TerminalDrained(receipt)) => {
                    panic!("PTY became terminal before yielding expected bytes: {receipt:?}")
                }
                Err(BrokerError::ProxyWouldBlock) => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("PTY output pump failed: {error}"),
            }
        }
        panic!("timed out waiting for PTY output");
    }

    #[derive(Clone, Copy)]
    enum TerminalReadMode {
        Zero,
        Eio,
    }

    struct TerminalTestReader {
        fd: fs::File,
        reads: Arc<AtomicUsize>,
        mode: TerminalReadMode,
    }

    impl Read for TerminalTestReader {
        fn read(&mut self, _output: &mut [u8]) -> std::io::Result<usize> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            match self.mode {
                TerminalReadMode::Zero => Ok(0),
                TerminalReadMode::Eio => Err(std::io::Error::from_raw_os_error(libc::EIO)),
            }
        }
    }

    impl AsFd for TerminalTestReader {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }

    #[test]
    fn pinned_catalog_reconciles_wal_ahead_of_head_without_second_spawn_authority() {
        let directory = private_catalog_directory();
        let sentinel = directory.path().join("unused-spawn-sentinel");
        let authority = authority(id(880), id(881), id(882), id(883), 0x81, 0x82);
        let payload = command_payload("printf unexpected", &sentinel);
        let identity = wal_identity(binding_for(&payload, &authority));
        let authenticator = wal_authenticator(0x91);
        let catalog = BrokerSpawnWalCatalogV1::open(directory.path().to_path_buf())
            .expect("open empty pinned broker catalog");
        let mut journal = catalog
            .create_spawn_journal(identity, authenticator.clone())
            .expect("create catalog-managed Spawn WAL");
        journal
            .append_intent_and_sync()
            .expect("synchronize catalog-managed intent");
        let attempt_id = id(884);
        journal.inject_fault(BrokerSpawnWalInjectedFault::AfterWalSyncBeforeHead);
        assert!(matches!(
            journal.begin_spawn_attempt_and_sync(attempt_id),
            Err(BrokerSpawnWalError::Io(_))
        ));
        assert!(journal.is_poisoned());
        drop(journal);
        drop(catalog);

        let recovered_catalog = BrokerSpawnWalCatalogV1::open(directory.path().to_path_buf())
            .expect("reopen exact pinned broker catalog");
        let mut journals = recovered_catalog
            .recover_all(&authenticator)
            .expect("authenticate and reconcile complete catalog");
        assert_eq!(journals.len(), 1);
        let mut recovered = journals.pop().expect("one recovered journal");
        let status = recovered.status();
        assert_eq!(status.identity, identity);
        assert_eq!(status.phase, Some(BrokerSpawnWalPhaseV1::Attempted));
        assert_eq!(status.attempt_id, Some(attempt_id));
        assert!(!status.append_authority_withheld);
        assert!(!status.head_reconciliation_required);
        assert!(status.durable_protocol_fence().unwrap().is_some());
        assert!(matches!(
            recovered
                .begin_spawn_attempt_and_sync(attempt_id)
                .expect("recover exact lost attempt reply"),
            BrokerSpawnAttemptAdmissionV1::Reconciled(replayed)
                if replayed.phase == Some(BrokerSpawnWalPhaseV1::Attempted)
        ));
    }

    #[test]
    fn pinned_catalog_rejects_second_owner_unknown_orphan_and_insecure_inode() {
        let owned_directory = private_catalog_directory();
        let owner = BrokerSpawnWalCatalogV1::open(owned_directory.path().to_path_buf())
            .expect("first catalog owner");
        assert!(matches!(
            BrokerSpawnWalCatalogV1::open(owned_directory.path().to_path_buf()),
            Err(BrokerSpawnWalError::CatalogAlreadyOwned)
        ));
        drop(owner);

        let unknown_directory = private_catalog_directory();
        let unknown = unknown_directory.path().join("unexpected-state");
        let unknown_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&unknown)
            .expect("create unknown catalog entry");
        drop(unknown_file);
        assert!(matches!(
            BrokerSpawnWalCatalogV1::open(unknown_directory.path().to_path_buf()),
            Err(BrokerSpawnWalError::UnexpectedCatalogEntry)
        ));

        let orphan_directory = private_catalog_directory();
        let orphan_name = broker_spawn_catalog_wal_name(id(885));
        let orphan = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(orphan_directory.path().join(orphan_name))
            .expect("create orphan WAL name");
        drop(orphan);
        assert!(matches!(
            BrokerSpawnWalCatalogV1::open(orphan_directory.path().to_path_buf()),
            Err(BrokerSpawnWalError::IncompleteCatalogPair)
        ));

        let linked_directory = private_catalog_directory();
        let sentinel = linked_directory.path().join("unused-spawn-sentinel");
        let authority = authority(id(886), id(887), id(888), id(889), 0x83, 0x84);
        let payload = command_payload("printf unexpected", &sentinel);
        let identity = wal_identity(binding_for(&payload, &authority));
        let authenticator = wal_authenticator(0x92);
        let catalog = BrokerSpawnWalCatalogV1::open(linked_directory.path().to_path_buf())
            .expect("open catalog before link mutation");
        let journal = catalog
            .create_spawn_journal(identity, authenticator.clone())
            .expect("create pair before link mutation");
        let wal_name = broker_spawn_catalog_wal_name(identity.journal_id());
        fs::hard_link(
            linked_directory.path().join(&wal_name),
            linked_directory.path().join("retained-hard-link-evidence"),
        )
        .expect("create hard-link mutation");
        drop(journal);
        assert!(matches!(
            catalog.recover_all(&authenticator),
            Err(BrokerSpawnWalError::UnexpectedCatalogEntry
                | BrokerSpawnWalError::InsecureCatalogIdentity)
        ));

        let target_directory = private_catalog_directory();
        let link_parent = private_catalog_directory();
        let link_path = link_parent.path().join("catalog-link");
        symlink(target_directory.path(), &link_path).expect("create catalog symlink mutation");
        assert!(matches!(
            BrokerSpawnWalCatalogV1::open(link_path),
            Err(BrokerSpawnWalError::InsecureCatalogIdentity)
        ));
    }

    #[test]
    fn guardian_manifest_enables_only_the_safe_linux_pidfd_process_feature() {
        let manifest = include_str!("../Cargo.toml");
        assert!(manifest.contains("rustix = { workspace = true, features = [\"process\"] }"));
        assert!(!manifest.contains("sysinfo.workspace"));
        assert!(!manifest.contains("passfd.workspace"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_child_incarnation_is_stable_live_and_rejects_reaped_child() {
        assert_eq!(
            parse_linux_proc_stat(
                b"77 (name ) with spaces) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19\n"
            )
            .expect("parse process name containing a closing parenthesis"),
            (b'S', 19)
        );
        assert!(parse_linux_proc_stat(b"77 malformed").is_err());
        assert!(
            parse_linux_proc_stat(b"77 (name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 0\n")
                .is_err()
        );

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open PTY for pidfd child proof");
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("sleep 30");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn live pidfd test child");
        drop(pair.slave);
        let nonce = id(890);
        let first = BrokerVerifiedKernelChildV1::verify_spawned_child(&mut *child, nonce)
            .expect("verify exact live child with pidfd");
        let first_identity = first.identity();
        assert_eq!(first_identity.process_id(), child.process_id().unwrap());
        assert_eq!(first_identity.broker_child_nonce(), nonce);
        assert_ne!(first_identity.kernel_start_identity_digest(), [0; 32]);
        let second = BrokerVerifiedKernelChildV1::verify_spawned_child(&mut *child, nonce)
            .expect("repeat exact live child verification");
        assert_eq!(second.identity(), first_identity);
        assert!(matches!(
            BrokerVerifiedKernelChildV1::verify_spawned_child(&mut *child, Uuid::nil()),
            Err(BrokerChildIncarnationError::InvalidIdentity)
        ));
        child.kill().expect("kill pidfd test child");
        child.wait().expect("reap pidfd test child");
        assert!(matches!(
            BrokerVerifiedKernelChildV1::verify_spawned_child(&mut *child, id(891)),
            Err(BrokerChildIncarnationError::ChildNotRunning)
        ));
        drop(pair.master);
    }

    #[test]
    fn pre_adoption_authenticated_eof_cannot_create_a_user_child() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("spawned");
        let auth = authority(id(1), id(2), id(3), id(4), 0x41, 0x42);
        let payload = command_payload("printf x >>\"$BROKER_SENTINEL\"", &sentinel);
        let binding = binding_for(&payload, &auth);
        let (prepared, control) =
            prepare_for_test(payload, binding, &auth, BrokerResourceLimitsV1::default())
                .expect("prepare child-free PTY");
        assert_eq!(
            prepared.resource_usage(),
            BrokerResourceUsageV1 {
                broker_pty_descriptors: 3,
                child_handles: 0,
                live_guardian_leases: 0,
                retained_spawn_payload_bytes: binding.spawn_payload_bytes,
                buffered_output_bytes: 0,
                max_buffered_output_bytes: BROKER_DEFAULT_MAX_BUFFERED_OUTPUT_BYTES,
                fenced_attachment_tombstones: 0,
                max_fenced_attachment_tombstones: usize::try_from(
                    BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS
                )
                .expect("handoff limit fits usize")
                    + 1,
                completed_successor_handoffs: 0,
            }
        );

        let receipt = prepared
            .abort_after_authenticated_control_eof(control)
            .expect("abort exact prepared authority");
        assert_eq!(receipt.durable_pane_id, binding.durable_pane_id);
        thread::sleep(Duration::from_millis(50));
        assert!(!sentinel.exists(), "pre-adoption abort spawned a child");
    }

    #[test]
    fn durable_commit_spawns_once_and_successor_keeps_exact_child_and_transcript() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("spawn-count");
        let auth = authority(id(21), id(22), id(23), id(24), 0x51, 0x52);
        let script = concat!(
            "printf S >>\"$BROKER_SENTINEL\"; ",
            "IFS= read -r first; printf 'first:%s\\n' \"$first\"; ",
            "sleep 0.05; printf 'gap-output\\n'; ",
            "if IFS= read -r second; then ",
            "printf 'second:%s\\n' \"$second\"; printf T >>\"$BROKER_SENTINEL\"; ",
            "else printf E >>\"$BROKER_SENTINEL\"; fi"
        );
        let payload = command_payload(script, &sentinel);
        let binding = binding_for(&payload, &auth);
        let (prepared, control) = prepare_for_test(
            payload,
            binding,
            &auth,
            BrokerResourceLimitsV1::new(GUARDIAN_MAX_PAYLOAD_BYTES, 2).unwrap(),
        )
        .expect("prepare broker pane");
        let BrokerAdoptionV1 {
            mut pane,
            attachment,
        } = commit_for_test(prepared, control, binding).expect("durable commit");
        let child_identity = pane.child_identity();
        let first_attachment = attachment.identity();
        proxy_write(&mut pane, &attachment, b"alpha\n");
        let first_output = read_until(&mut pane, &attachment, b"first:alpha");
        assert!(
            first_output
                .windows(11)
                .any(|window| window == b"first:alpha")
        );
        let first_output_sequence = pane.status().output_sequence;
        assert_eq!(pane.resource_usage().buffered_output_bytes, 0);

        // Model a write already queued behind the proxy event loop. Rotation
        // must invalidate it at the second, immediately-before-effect check.
        let stale_write = pane
            .admit_proxy_write(&attachment, b"stale\n")
            .expect("admit write before rotation");
        let stale_resize = pane
            .admit_proxy_resize(
                &attachment,
                PtySize {
                    rows: 42,
                    cols: 132,
                    pixel_width: 8,
                    pixel_height: 16,
                },
            )
            .expect("admit resize before rotation");
        let stale_read = pane
            .admit_proxy_read(&attachment, 1)
            .expect("admit read before rotation");
        let stale_ack = pane
            .admit_proxy_output_ack(&attachment, pane.buffer_start_sequence)
            .expect("admit output acknowledgement before rotation");

        let eof = BrokerAuthenticatedControlEofV1::from_authenticated_transport_close(
            attachment.identity(),
        );
        assert_eq!(
            pane.observe_authenticated_control_eof(eof),
            Ok(BrokerControlEofOutcomeV1::AwaitingSuccessor { next_generation: 2 })
        );
        assert_eq!(
            pane.execute_proxy_write(stale_write, b"stale\n"),
            Err(BrokerError::StaleProxyLease),
            "queued predecessor write crossed the generation fence"
        );
        assert_eq!(
            pane.execute_proxy_resize(stale_resize),
            Err(BrokerError::StaleProxyLease),
            "queued predecessor resize crossed the generation fence"
        );
        assert_eq!(
            pane.execute_proxy_read(stale_read, &mut [0_u8; 1]),
            Err(BrokerError::StaleProxyLease),
            "queued predecessor read crossed the generation fence"
        );
        assert_eq!(
            pane.execute_proxy_output_ack(stale_ack),
            Err(BrokerError::StaleProxyLease),
            "queued predecessor output acknowledgement crossed the generation fence"
        );
        assert_eq!(pane.resource_usage().broker_pty_descriptors, 3);
        assert_eq!(pane.resource_usage().live_guardian_leases, 0);
        assert_eq!(pane.resource_usage().fenced_attachment_tombstones, 1);
        pump_until_sequence_advances(&mut pane, first_output_sequence);
        assert_eq!(
            fs::read(&sentinel).expect("read pre-handoff phase"),
            b"S",
            "dropping the old lease injected input or EOF into the child"
        );
        assert!(
            pane.child_is_running_for_test()
                .expect("poll child after lease detach"),
            "dropping the old lease terminated the retained child"
        );
        assert_eq!(pane.child_identity(), child_identity);

        let successor = authority(id(21), id(25), id(26), id(27), 0x61, 0x62);
        let handoff_id = id(28);
        let handoff = BrokerSuccessorHandoffAuthorityV1 {
            broker_incarnation: id(21),
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            handoff_id,
            predecessor: first_attachment,
            successor: successor.owner,
        };
        let successor_attachment = match pane
            .attach_successor(handoff)
            .expect("attach fenced successor")
        {
            BrokerSuccessorAttachOutcomeV1::Attached(attachment) => attachment,
            BrokerSuccessorAttachOutcomeV1::RecoveredExistingLease { .. } => {
                panic!("first handoff cannot already be applied")
            }
        };
        assert_eq!(successor_attachment.identity().lease_generation(), 2);
        assert_eq!(pane.child_identity(), child_identity);
        assert_eq!(pane.resource_usage().broker_pty_descriptors, 3);
        assert_eq!(pane.resource_usage().live_guardian_leases, 1);

        let successor_identity = successor_attachment.identity();
        let lost_reply_retry = BrokerSuccessorHandoffAuthorityV1 {
            broker_incarnation: id(21),
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            handoff_id,
            predecessor: first_attachment,
            successor: successor.owner,
        };
        let recovered_attachment = match pane
            .attach_successor(lost_reply_retry)
            .expect("recover already-applied handoff acknowledgement")
        {
            BrokerSuccessorAttachOutcomeV1::RecoveredExistingLease { attachment, status } => {
                assert_eq!(status.completed_successor_handoffs, 1);
                attachment
            }
            BrokerSuccessorAttachOutcomeV1::Attached(_) => {
                panic!("lost-reply retry minted a second lease")
            }
        };
        assert_eq!(recovered_attachment.identity(), successor_identity);
        assert_eq!(pane.resource_usage().live_guardian_leases, 1);
        let successor_attachment = recovered_attachment;

        let delayed_predecessor_eof =
            BrokerAuthenticatedControlEofV1::from_authenticated_transport_close(first_attachment);
        assert_eq!(
            pane.observe_authenticated_control_eof(delayed_predecessor_eof),
            Ok(BrokerControlEofOutcomeV1::AlreadyObserved),
            "a retried predecessor EOF quarantined the valid successor"
        );
        assert_eq!(pane.status().lifecycle, BrokerPaneLifecycleV1::Active);
        assert_eq!(pane.resource_usage().live_guardian_leases, 1);

        let gap_output = read_until(&mut pane, &successor_attachment, b"gap-output");
        assert!(
            gap_output
                .windows(b"gap-output".len())
                .any(|window| window == b"gap-output")
        );
        proxy_write(&mut pane, &successor_attachment, b"beta\n");
        let second_output = read_until(&mut pane, &successor_attachment, b"second:beta");
        assert!(
            second_output
                .windows(11)
                .any(|window| window == b"second:beta")
        );
        assert!(pane.status().output_sequence > first_output_sequence);
        assert!(pane.wait_for_test().expect("wait for child").success());
        assert_eq!(
            fs::read(&sentinel).expect("read spawn sentinel"),
            b"ST",
            "handoff respawned or advanced the child before successor input"
        );
        assert_eq!(pane.child_identity().process_id, child_identity.process_id);
        assert_eq!(pane.status().completed_successor_handoffs, 1);
    }

    #[test]
    fn canonical_payload_geometry_and_build_mutations_fail_before_spawn() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("spawned");
        let auth = authority(id(31), id(32), id(33), id(34), 0x71, 0x72);
        let make_payload = || command_payload("printf x >>\"$BROKER_SENTINEL\"", &sentinel);
        let baseline = make_payload();
        let binding = binding_for(&baseline, &auth);

        let mut bad_digest = binding;
        bad_digest.spawn_payload_digest[0] ^= 0x80;
        assert_eq!(
            prepare_for_test(
                make_payload(),
                bad_digest,
                &auth,
                BrokerResourceLimitsV1::default(),
            )
            .err(),
            Some(BrokerError::SpawnPayloadBindingMismatch)
        );

        let mut bad_geometry = binding;
        bad_geometry.cols += 1;
        assert_eq!(
            prepare_for_test(
                make_payload(),
                bad_geometry,
                &auth,
                BrokerResourceLimitsV1::default(),
            )
            .err(),
            Some(BrokerError::SpawnPayloadBindingMismatch)
        );

        let wrong_build_auth = authority(id(31), id(32), id(33), id(34), 0x73, 0x72);
        assert_eq!(
            prepare_for_test(
                make_payload(),
                binding,
                &wrong_build_auth,
                BrokerResourceLimitsV1::default(),
            )
            .err(),
            Some(BrokerError::AuthenticatedAuthorityMismatch)
        );
        thread::sleep(Duration::from_millis(50));
        assert!(!sentinel.exists(), "a rejected mutation spawned a child");
    }

    #[test]
    fn conflicting_handoff_sticky_quarantines_without_respawn() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("spawn-count");
        let auth = authority(id(41), id(42), id(43), id(44), 0x21, 0x22);
        let payload = command_payload(
            "printf x >>\"$BROKER_SENTINEL\"; IFS= read -r ignored",
            &sentinel,
        );
        let binding = binding_for(&payload, &auth);
        let (prepared, control) =
            prepare_for_test(payload, binding, &auth, BrokerResourceLimitsV1::default())
                .expect("prepare pane");
        let BrokerAdoptionV1 {
            mut pane,
            attachment,
        } = commit_for_test(prepared, control, binding).expect("commit pane");
        let attachment_identity = attachment.identity();
        let eof = BrokerAuthenticatedControlEofV1::from_authenticated_transport_close(
            attachment_identity,
        );
        pane.observe_authenticated_control_eof(eof)
            .expect("observe exact EOF");

        let successor = authority(id(41), id(45), id(46), id(47), 0x23, 0x24);
        let mut wrong_predecessor = attachment_identity;
        wrong_predecessor.lease_generation = 9;
        let conflict = BrokerSuccessorHandoffAuthorityV1 {
            broker_incarnation: id(41),
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            handoff_id: id(48),
            predecessor: wrong_predecessor,
            successor: successor.owner,
        };
        assert_eq!(
            pane.attach_successor(conflict).err(),
            Some(BrokerError::ConflictingSuccessorHandoff)
        );
        assert_eq!(
            pane.status().lifecycle,
            BrokerPaneLifecycleV1::Quarantined(
                BrokerQuarantineReasonV1::ConflictingSuccessorHandoff
            )
        );

        let retry = BrokerSuccessorHandoffAuthorityV1 {
            broker_incarnation: id(41),
            durable_pane_id: binding.durable_pane_id,
            spawn_effect_id: binding.spawn_effect_id,
            handoff_id: id(49),
            predecessor: attachment_identity,
            successor: successor.owner,
        };
        assert_eq!(
            pane.attach_successor(retry).err(),
            Some(BrokerError::Quarantined)
        );
        thread::sleep(Duration::from_millis(50));
        assert_eq!(fs::read(&sentinel).expect("spawn sentinel"), b"x");
        pane.terminate_and_wait_for_test();
    }

    #[test]
    fn output_read_replays_lost_reply_and_ack_releases_bounded_window() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("unused");
        let auth = authority(id(51), id(52), id(53), id(54), 0x31, 0x32);
        let payload = command_payload("printf 'abcdefghijklmnop'", &sentinel);
        let binding = binding_for(&payload, &auth);
        let limits = BrokerResourceLimitsV1::with_proxy_bounds(GUARDIAN_MAX_PAYLOAD_BYTES, 1, 8, 8)
            .expect("small valid proxy bounds");
        let (prepared, control) =
            prepare_for_test(payload, binding, &auth, limits).expect("prepare bounded output pane");
        let BrokerAdoptionV1 {
            mut pane,
            attachment,
        } = commit_for_test(prepared, control, binding).expect("commit bounded output pane");

        while pane.resource_usage().buffered_output_bytes < limits.max_buffered_output_bytes() {
            let _ = pump_until_drained(&mut pane);
        }
        assert_eq!(
            pane.pump_ready_output(),
            Err(BrokerError::OutputBufferFull),
            "full unacknowledged output window did not activate backpressure"
        );

        let first_permit = pane
            .admit_proxy_read(&attachment, 8)
            .expect("admit first delivery");
        let mut first_bytes = [0_u8; 8];
        let first = pane
            .execute_proxy_read(first_permit, &mut first_bytes)
            .expect("execute first delivery");
        assert_eq!(&first_bytes[..first.bytes_read], b"abcdefgh");

        // Model effect success followed by response loss. A fresh read must
        // replay the exact unacknowledged range instead of skipping it.
        let retry_permit = pane
            .admit_proxy_read(&attachment, 8)
            .expect("admit lost-reply retry");
        let mut retry_bytes = [0_u8; 8];
        let retry = pane
            .execute_proxy_read(retry_permit, &mut retry_bytes)
            .expect("execute lost-reply retry");
        assert_ne!(retry.operation_id, first.operation_id);
        assert_eq!(retry.output_sequence_start, first.output_sequence_start);
        assert_eq!(retry.output_sequence_end, first.output_sequence_end);
        assert_eq!(
            &retry_bytes[..retry.bytes_read],
            &first_bytes[..first.bytes_read]
        );

        let ack = pane
            .admit_proxy_output_ack(&attachment, retry.output_sequence_end)
            .expect("admit replayed delivery acknowledgement");
        pane.execute_proxy_output_ack(ack)
            .expect("commit replayed delivery acknowledgement");
        assert_eq!(pane.resource_usage().buffered_output_bytes, 0);
        let ack_retry = pane
            .admit_proxy_output_ack(&attachment, retry.output_sequence_end)
            .expect("admit lost acknowledgement reply retry");
        pane.execute_proxy_output_ack(ack_retry)
            .expect("acknowledgement retry is idempotent");
        assert_eq!(pane.resource_usage().buffered_output_bytes, 0);

        while pane.status().output_sequence < 16 {
            let receipt = pump_until_drained(&mut pane);
            let permit = pane
                .admit_proxy_read(&attachment, receipt.bytes_drained)
                .expect("admit next bounded delivery");
            let mut bytes = [0_u8; 8];
            let delivered = pane
                .execute_proxy_read(permit, &mut bytes)
                .expect("execute next bounded delivery");
            let ack = pane
                .admit_proxy_output_ack(&attachment, delivered.output_sequence_end)
                .expect("admit next output acknowledgement");
            pane.execute_proxy_output_ack(ack)
                .expect("commit next output acknowledgement");
        }
        assert!(
            pane.status().output_sequence
                > u64::try_from(limits.max_buffered_output_bytes()).expect("buffer bound fits u64"),
            "lifetime delivery incorrectly stalled at the bounded unacknowledged window"
        );
        assert_eq!(pane.resource_usage().buffered_output_bytes, 0);
        assert!(
            pane.wait_for_test()
                .expect("wait for output child")
                .success()
        );
    }

    #[test]
    fn zero_and_eio_terminal_reads_are_recorded_once_without_busy_spin() {
        for (mode, expected_reason) in [
            (
                TerminalReadMode::Zero,
                BrokerOutputTerminalReasonV1::ZeroLengthRead,
            ),
            (
                TerminalReadMode::Eio,
                BrokerOutputTerminalReasonV1::PtyIoClosed,
            ),
        ] {
            let temp = tempfile::tempdir().expect("test directory");
            let sentinel = temp.path().join("unused");
            let auth = authority(id(61), id(62), id(63), id(64), 0x41, 0x42);
            let payload = command_payload("IFS= read -r ignored", &sentinel);
            let binding = binding_for(&payload, &auth);
            let (prepared, control) =
                prepare_for_test(payload, binding, &auth, BrokerResourceLimitsV1::default())
                    .expect("prepare terminal-read pane");
            let BrokerAdoptionV1 {
                mut pane,
                attachment,
            } = commit_for_test(prepared, control, binding).expect("commit terminal-read pane");
            let reads = Arc::new(AtomicUsize::new(0));
            pane.proxy_reader = Some(Box::new(TerminalTestReader {
                fd: fs::File::open("/dev/null").expect("open inert test descriptor"),
                reads: Arc::clone(&reads),
                mode,
            }));

            let first = pane.pump_ready_output().expect("observe terminal read");
            let BrokerOutputPumpOutcomeV1::TerminalDrained(first) = first else {
                panic!("terminal reader returned ordinary bytes")
            };
            assert_eq!(first.reason, expected_reason);
            assert!(first.newly_observed);
            assert_eq!(reads.load(Ordering::Acquire), 1);
            assert_eq!(pane.status().output_terminal_reason, Some(expected_reason));
            assert_eq!(pane.resource_usage().broker_pty_descriptors, 2);

            let repeated = pane
                .pump_ready_output()
                .expect("repeat terminal query is idempotent");
            let BrokerOutputPumpOutcomeV1::TerminalDrained(repeated) = repeated else {
                panic!("terminal state resumed reading")
            };
            assert_eq!(repeated.reason, expected_reason);
            assert!(!repeated.newly_observed);
            assert_eq!(reads.load(Ordering::Acquire), 1);
            assert_eq!(
                pane.status().output_child_exit_observed,
                repeated.child_exit_observed
            );
            let read = pane
                .admit_proxy_read(&attachment, 1)
                .expect("admit terminal output query");
            assert_eq!(
                pane.execute_proxy_read(read, &mut [0_u8; 1]),
                Err(BrokerError::ProxyOutputTerminalDrained)
            );
            pane.terminate_and_wait_for_test();
        }
    }

    #[test]
    fn durable_spawn_wal_query_ack_lifecycle_never_reinvokes_effect() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("unused");
        let auth = authority(id(71), id(72), id(73), id(74), 0x51, 0x52);
        let payload = command_payload("true", &sentinel);
        let binding = binding_for(&payload, &auth);
        let identity = wal_identity(binding);
        let authenticator = wal_authenticator(0x81);
        let (mut journal, wal_path, head_path) =
            create_test_spawn_journal(temp.path(), identity, authenticator.clone());
        assert_eq!(
            journal
                .status()
                .durable_protocol_fence()
                .expect("validate empty WAL identity"),
            None,
            "authenticated setup headers alone fenced an unattempted Spawn"
        );

        let intent = journal
            .append_intent_and_sync()
            .expect("synchronize Spawn intent");
        assert_eq!(intent.phase(), BrokerSpawnWalPhaseV1::Intent);
        let durable_fence = journal
            .status()
            .durable_protocol_fence()
            .expect("project durable Spawn fence")
            .expect("Intent creates the global Spawn fence");
        assert_eq!(durable_fence.mux_incarnation(), binding.mux_incarnation);
        assert_eq!(durable_fence.spawn_effect_id(), binding.spawn_effect_id);
        assert_eq!(durable_fence.durable_pane_id(), binding.durable_pane_id);
        assert_eq!(durable_fence.origin_request_id(), binding.origin_request_id);
        assert_eq!(
            durable_fence.spawn_payload_digest(),
            binding.spawn_payload_digest
        );
        assert_eq!(
            journal.append_intent_and_sync().expect("replay intent"),
            intent,
            "an exact Intent retry appended another record"
        );

        let attempt_id = id(902);
        let permit = match journal
            .begin_spawn_attempt_and_sync(attempt_id)
            .expect("synchronize Spawn Attempt")
        {
            BrokerSpawnAttemptAdmissionV1::Authorized(permit) => permit,
            BrokerSpawnAttemptAdmissionV1::Reconciled(_) => {
                panic!("first Attempt did not yield one-shot authority")
            }
        };
        let effect_invocations = AtomicUsize::new(0);
        let child_identity = test_kernel_child(12_345);
        let execution = permit.invoke_once(|| {
            effect_invocations.fetch_add(1, Ordering::AcqRel);
            Ok::<_, ()>(("retained-child-handle", child_identity))
        });
        let (retained, observation) = match execution {
            BrokerSpawnAttemptExecutionV1::EffectSucceeded { value, observation } => {
                (value, observation)
            }
            BrokerSpawnAttemptExecutionV1::OutcomeIndeterminate { .. } => {
                panic!("valid child identity became indeterminate")
            }
        };
        assert_eq!(retained, "retained-child-handle");
        assert_eq!(effect_invocations.load(Ordering::Acquire), 1);

        match journal
            .begin_spawn_attempt_and_sync(attempt_id)
            .expect("recover lost Attempt reply")
        {
            BrokerSpawnAttemptAdmissionV1::Reconciled(status) => {
                assert_eq!(status.phase, Some(BrokerSpawnWalPhaseV1::Attempted));
                assert!(status.spawn_outcome_is_indeterminate());
            }
            BrokerSpawnAttemptAdmissionV1::Authorized(_) => {
                panic!("lost Attempt reply minted a second callback permit")
            }
        }
        assert_eq!(effect_invocations.load(Ordering::Acquire), 1);

        let observed = journal
            .append_spawn_observed_and_sync(*observation)
            .expect("synchronize exact child observation");
        assert_eq!(observed.phase(), BrokerSpawnWalPhaseV1::SpawnObserved);
        let query = journal.status();
        assert_eq!(query.child_identity, Some(child_identity));
        assert!(query.fences_legacy_spawn());
        assert!(!query.spawn_outcome_is_indeterminate());
        match journal
            .begin_spawn_attempt_and_sync(attempt_id)
            .expect("query observed Spawn")
        {
            BrokerSpawnAttemptAdmissionV1::Reconciled(status) => {
                assert_eq!(status.child_identity, Some(child_identity));
            }
            BrokerSpawnAttemptAdmissionV1::Authorized(_) => {
                panic!("observed Spawn query minted a second callback permit")
            }
        }

        let ack_id = id(903);
        let acknowledged = journal
            .acknowledge_spawn_reply_and_sync(ack_id, child_identity)
            .expect("synchronize Spawn reply acknowledgement");
        assert_eq!(
            acknowledged.phase(),
            BrokerSpawnWalPhaseV1::ReplyAcknowledged
        );
        assert_eq!(
            journal
                .acknowledge_spawn_reply_and_sync(ack_id, child_identity)
                .expect("recover lost acknowledgement reply"),
            acknowledged,
            "exact acknowledgement retry appended a second record"
        );
        assert_eq!(journal.status().committed_records, 4);
        drop(journal);

        let mut recovered =
            reopen_test_spawn_journal(&wal_path, &head_path, identity, authenticator);
        let recovered_status = recovered.status();
        assert_eq!(
            recovered_status.phase,
            Some(BrokerSpawnWalPhaseV1::ReplyAcknowledged)
        );
        assert_eq!(recovered_status.reply_ack_id, Some(ack_id));
        assert_eq!(recovered_status.child_identity, Some(child_identity));
        assert_eq!(
            recovered_status
                .durable_protocol_fence()
                .expect("project recovered durable Spawn fence"),
            Some(durable_fence)
        );
        assert!(recovered_status.append_authority_withheld);
        let revalidation = revalidate_recovered_test_journal(&recovered);
        recovered
            .reconcile_recovered_head_and_activate(revalidation)
            .expect("activate exact recovered WAL/head pair");
        assert!(!recovered.status().append_authority_withheld);
    }

    #[test]
    fn wal_ahead_of_head_crash_cut_reconciles_without_second_spawn_permit() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("unused");
        let auth = authority(id(81), id(82), id(83), id(84), 0x61, 0x62);
        let payload = command_payload("true", &sentinel);
        let identity = wal_identity(binding_for(&payload, &auth));
        let authenticator = wal_authenticator(0x82);
        let (mut journal, wal_path, head_path) =
            create_test_spawn_journal(temp.path(), identity, authenticator.clone());
        journal.inject_fault(BrokerSpawnWalInjectedFault::AfterWalSyncBeforeHead);
        assert!(matches!(
            journal.append_intent_and_sync(),
            Err(BrokerSpawnWalError::Io(_))
        ));
        assert!(journal.is_poisoned());
        drop(journal);

        let mut recovered =
            reopen_test_spawn_journal(&wal_path, &head_path, identity, authenticator);
        let pending = recovered.status();
        assert_eq!(pending.phase, Some(BrokerSpawnWalPhaseV1::Intent));
        assert!(pending.head_reconciliation_required);
        assert!(pending.append_authority_withheld);
        assert!(matches!(
            recovered.begin_spawn_attempt_and_sync(id(904)),
            Err(BrokerSpawnWalError::RecoveryAuthorityUnavailable)
        ));
        let revalidation = revalidate_recovered_test_journal(&recovered);
        recovered
            .reconcile_recovered_head_and_activate(revalidation)
            .expect("publish missing local head anchor");
        assert!(!recovered.status().head_reconciliation_required);

        let permit = match recovered
            .begin_spawn_attempt_and_sync(id(904))
            .expect("one Attempt after reconciled Intent")
        {
            BrokerSpawnAttemptAdmissionV1::Authorized(permit) => permit,
            BrokerSpawnAttemptAdmissionV1::Reconciled(_) => {
                panic!("unattempted recovered Intent lost its one callback authority")
            }
        };
        let calls = AtomicUsize::new(0);
        let execution = permit.invoke_once(|| {
            calls.fetch_add(1, Ordering::AcqRel);
            Err::<((), BrokerKernelChildIdentityV1), _>("ambiguous spawn callback")
        });
        assert!(matches!(
            execution,
            BrokerSpawnAttemptExecutionV1::OutcomeIndeterminate {
                retained_value: None
            }
        ));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        match recovered
            .begin_spawn_attempt_and_sync(id(904))
            .expect("query ambiguous Attempt")
        {
            BrokerSpawnAttemptAdmissionV1::Reconciled(status) => {
                assert!(status.spawn_outcome_is_indeterminate());
            }
            BrokerSpawnAttemptAdmissionV1::Authorized(_) => {
                panic!("ambiguous Spawn callback was authorized twice")
            }
        }
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn spawn_wal_prewrite_retry_torn_tail_key_rotation_and_head_rollback_fail_closed() {
        let temp = tempfile::tempdir().expect("test directory");
        let sentinel = temp.path().join("unused");
        let auth = authority(id(91), id(92), id(93), id(94), 0x71, 0x72);
        let payload = command_payload("true", &sentinel);
        let identity = wal_identity(binding_for(&payload, &auth));
        let authenticator = wal_authenticator(0x83);
        let (mut journal, wal_path, head_path) =
            create_test_spawn_journal(temp.path(), identity, authenticator.clone());

        journal.inject_fault(BrokerSpawnWalInjectedFault::BeforeWalWrite);
        assert!(matches!(
            journal.append_intent_and_sync(),
            Err(BrokerSpawnWalError::Io(_))
        ));
        assert!(!journal.is_poisoned());
        journal
            .append_intent_and_sync()
            .expect("retry before any WAL write");
        let _permit = match journal
            .begin_spawn_attempt_and_sync(id(905))
            .expect("synchronize second lifecycle record")
        {
            BrokerSpawnAttemptAdmissionV1::Authorized(permit) => permit,
            BrokerSpawnAttemptAdmissionV1::Reconciled(_) => panic!("first Attempt reconciled"),
        };
        drop(journal);

        assert!(matches!(
            BrokerSpawnJournalV1::open(
                open_existing_test_file(&wal_path),
                open_existing_test_file(&head_path),
                identity,
                wal_authenticator(0x84),
            ),
            Err(BrokerSpawnWalError::KeyIdentityMismatch)
        ));

        let tamper_dir = temp.path().join("tamper");
        fs::create_dir(&tamper_dir).expect("create tamper fixture directory");
        let (mut authenticated, tamper_wal_path, tamper_head_path) =
            create_test_spawn_journal(&tamper_dir, identity, authenticator.clone());
        authenticated
            .append_intent_and_sync()
            .expect("commit authenticated tamper fixture");
        drop(authenticated);
        let mut tampered = open_existing_test_file(&tamper_wal_path);
        tampered
            .seek(SeekFrom::Start(BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64 - 1))
            .expect("seek authenticated header tag");
        let mut byte = [0_u8; 1];
        tampered
            .read_exact(&mut byte)
            .expect("read authenticated header tag byte");
        byte[0] ^= 0x80;
        tampered
            .seek(SeekFrom::Start(BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64 - 1))
            .expect("rewind authenticated header tag");
        tampered
            .write_all(&byte)
            .expect("tamper authenticated header tag");
        tampered.sync_all().expect("sync authenticated tamper");
        drop(tampered);
        assert!(matches!(
            BrokerSpawnJournalV1::open(
                open_existing_test_file(&tamper_wal_path),
                open_existing_test_file(&tamper_head_path),
                identity,
                authenticator.clone(),
            ),
            Err(BrokerSpawnWalError::AuthenticationFailed)
        ));

        // A head rollback by two complete authenticated records is not a valid
        // writer crash cut. Recovery rejects it rather than inferring that the
        // Spawn effect was never attempted.
        open_existing_test_file(&head_path)
            .set_len(BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64)
            .expect("truncate temporary rollback fixture");
        assert!(matches!(
            BrokerSpawnJournalV1::open(
                open_existing_test_file(&wal_path),
                open_existing_test_file(&head_path),
                identity,
                authenticator.clone(),
            ),
            Err(BrokerSpawnWalError::HeadReconciliationGap)
        ));

        // Restore a separate exact fixture and append an incomplete physical
        // tail. Recovery reports and seals it without truncating evidence.
        let torn_dir = temp.path().join("torn");
        fs::create_dir(&torn_dir).expect("create torn fixture directory");
        let (mut torn, torn_wal_path, torn_head_path) =
            create_test_spawn_journal(&torn_dir, identity, authenticator.clone());
        torn.append_intent_and_sync()
            .expect("commit torn fixture prefix");
        drop(torn);
        let mut external = OpenOptions::new()
            .append(true)
            .open(&torn_wal_path)
            .expect("open torn WAL fixture");
        external
            .write_all(&BROKER_SPAWN_WAL_RECORD_MAGIC[..3])
            .expect("append incomplete tail");
        external.sync_all().expect("sync incomplete tail");
        drop(external);
        let mut recovered =
            reopen_test_spawn_journal(&torn_wal_path, &torn_head_path, identity, authenticator);
        assert_eq!(
            recovered.status().tail,
            BrokerSpawnWalTailV1::Incomplete {
                wal_trailing_bytes: 3,
                head_trailing_bytes: 0,
            }
        );
        let revalidation = revalidate_recovered_test_journal(&recovered);
        assert!(matches!(
            recovered.reconcile_recovered_head_and_activate(revalidation),
            Err(BrokerSpawnWalError::IncompleteTail)
        ));
        assert_eq!(
            fs::metadata(&torn_wal_path)
                .expect("torn WAL metadata")
                .len(),
            BROKER_SPAWN_WAL_FILE_HEADER_BYTES_U64 + BROKER_SPAWN_WAL_RECORD_BYTES_U64 + 3,
            "recovery truncated preserved crash evidence"
        );
    }

    #[test]
    fn resource_limits_are_closed_and_bounded() {
        assert_eq!(
            BrokerResourceLimitsV1::new(0, 1),
            Err(BrokerError::CapacityExhausted)
        );
        assert_eq!(
            BrokerResourceLimitsV1::new(GUARDIAN_MAX_PAYLOAD_BYTES + 1, 1),
            Err(BrokerError::CapacityExhausted)
        );
        assert_eq!(
            BrokerResourceLimitsV1::new(1, BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS + 1),
            Err(BrokerError::CapacityExhausted)
        );
        assert_eq!(
            BrokerResourceLimitsV1::new(GUARDIAN_MAX_PAYLOAD_BYTES, 1)
                .expect("valid hard bounds")
                .max_successor_handoffs(),
            1
        );
    }
}
