//! Production-disabled PTY broker typestate foundation.
//!
//! This module models the process-local ownership and authority transitions
//! needed by a future separately spawned broker process. The broker retains
//! the sole PTY master and exposes bounded authenticated proxy operations;
//! guardians never receive a master descriptor. That is essential because an
//! `SCM_RIGHTS` transfer cannot be revoked and socket EOF cannot fence a master
//! already installed in a predecessor guardian.
//!
//! There is deliberately no transport or command-line activation. This
//! in-process typestate does **not** survive guardian `SIGKILL`, and catalog
//! Genesis admission below is durable pre-Spawn intent, never proof that a
//! child exists. Activation additionally requires a separately spawned
//! same-binary broker, its own durable prepare/admit/spawn/ack log keyed by
//! `spawn_effect_id` plus non-recycled OS child identity, Query/Ack recovery,
//! startup reconciliation of every crash cut, and a real cross-process crash
//! matrix. In particular, catalog-marker-before-spawn and
//! spawn-success-before-ack remain deliberately unresolved here.
//!
//! The ordering enforced here is:
//!
//! 1. validate an authenticated guardian connection and the exact canonical
//!    Spawn reservation;
//! 2. open the PTY and reserve one broker-owned master/reader/writer proxy, but
//!    create no child;
//! 3. consume the synchronously durable Genesis pre-Spawn intent;
//! 4. process-locally spawn once and issue one logical guardian lease;
//! 5. fence every proxy operation at admission and again immediately before
//!    effect, so rotation invalidates already-queued stale work;
//! 6. accept a successor only after authenticated connection EOF revoked the
//!    old logical proxy lease and an exact generation/build-fenced handoff.
//!
//! The durable broker recovery tranche must also reconstruct the Spawn fence
//! before accepting traffic and make every Spawn dispatch consult it. The
//! existing generic guardian protocol keeps reservation/effect/pane maps only
//! in memory; restart plus legacy Spawn replay can otherwise bypass this
//! typestate and create a second child for a broker-managed identity.

#![allow(dead_code)] // Activation is intentionally held for the cross-process tranche.

use crate::SealedAtomicBuildIdentity;
use crate::output::GuardianPublishedGenesisAdmissionPermitV1;
use mux::guardian_checkpoint::GuardianGenesisReservationIdentityV1;
use mux::guardian_protocol::{GUARDIAN_MAX_PAYLOAD_BYTES, GuardianSpawnPayload};
#[cfg(test)]
use portable_pty::ExitStatus;
use portable_pty::{Child, MasterPty, PollablePtyReader, PtyPair, PtySize, native_pty_system};
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use thiserror::Error;
use uuid::Uuid;

const BROKER_ABSOLUTE_MAX_SUCCESSOR_HANDOFFS: u32 = 1_024;
const BROKER_CATALOG_CHECKSUM_BYTES: usize = 32;
const BROKER_DEFAULT_MAX_PROXY_OPERATION_BYTES: usize = 64 * 1024;
const BROKER_DEFAULT_MAX_BUFFERED_OUTPUT_BYTES: usize = 1024 * 1024;
const BROKER_ABSOLUTE_MAX_BUFFERED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const BROKER_OUTPUT_PUMP_CHUNK_BYTES: usize = 8 * 1024;

fn is_pty_terminal_eio(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
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
            lease_generation: 0,
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
    use std::io::Read;
    use std::os::fd::{AsFd, BorrowedFd};
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
        let _detached_attachment = attachment;
        assert_eq!(
            pane.observe_authenticated_control_eof(eof),
            Ok(BrokerControlEofOutcomeV1::AwaitingSuccessor { next_generation: 1 })
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
        assert_eq!(successor_attachment.identity().lease_generation(), 1);
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
        let _superseded_successor_reply = successor_attachment;
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
        let _detached_attachment = attachment;
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
