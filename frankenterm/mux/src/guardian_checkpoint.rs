//! Exact parser/output boundary authority for durable guardian checkpoints.
//!
//! A terminal-state payload is safe to publish only when it describes the
//! model produced through one exact synchronized output-journal receipt and
//! the escape parser can be replaced by a fresh parser at that same boundary.
//! This module owns that cross-subsystem identity. It intentionally does not
//! serialize terminal state; the terminal checkpoint codec must first produce
//! and validate its own bounded semantic payload, then bind it to this value.

use crate::guardian_output_journal::{
    GuardianOutputAppendReceipt, GuardianOutputCipher, GuardianOutputSegmentIdentity,
};
use crate::pane::Pane;
use crate::{
    LiveParserCheckpointControl, LiveParserCheckpointError, PaneRegistrationGeneration,
    PaneRegistrationOperationLease,
};
use frankenterm_term::{
    RECOVERY_TERMINAL_REPLAY_SEMANTICS_ID, RecoveryTerminalCheckpointError,
    RecoveryTerminalCheckpointV2,
    terminalstate::checkpoint::{TerminalCheckpointLimits, TerminalCheckpointV2},
};
use sha2::{Digest as _, Sha256};
use std::convert::TryFrom;
use std::sync::{Arc, Weak};
use termwiz::escape::parser::RECOVERY_CHECKPOINT_PARSER_ID;
use termwiz::escape::{parser::RecoveryGroundBoundary, Action};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

const REPLAY_IDENTITY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-replay-identity.v1\0";
const TERMINAL_PAYLOAD_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-terminal-payload.v1\0";
const LIVE_PARSER_BOUNDARY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.live-parser-checkpoint-boundary.v1\0";
const OUTPUT_BOUNDARY_IDENTITY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-output-boundary-identity.v1\0";
const GENESIS_BOUNDARY_IDENTITY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-genesis-boundary-identity.v1\0";
const CHECKPOINT_ARTIFACT_IDENTITY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-artifact-identity.v1\0";
const CHECKPOINT_STAGE_RECORD_AEAD_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-phase-a-record.v1\0";
const CHECKPOINT_STAGE_PLAINTEXT_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-phase-a-plaintext.v1\0";
const CHECKPOINT_STAGE_RECORD_MAGIC: [u8; 8] = *b"FTGCPA01";

/// Version of the encrypted Phase-A checkpoint staging-record format.
pub const GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION: u32 = 1;
/// Exact fixed header size emitted beside one encrypted staging-record body.
pub const GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES: usize = 232;
/// Hard per-record plaintext admission bound shared with checkpoint uploads.
pub const GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES: u32 = 256 * 1024;

const CHECKPOINT_STAGE_CONTEXT_BYTES: usize = 184;
const CHECKPOINT_STAGE_KEY_ID_BYTES: usize = 8;
const CHECKPOINT_STAGE_NONCE_BYTES: usize = 24;
const CHECKPOINT_STAGE_AEAD_TAG_BYTES: usize = 16;
const CHECKPOINT_STAGE_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const CHECKPOINT_STAGE_MAX_CHUNKS: u32 = 1_024;

/// Version of the cross-subsystem checkpoint-boundary contract.
pub const GUARDIAN_CHECKPOINT_BOUNDARY_VERSION: u32 = 2;

#[derive(Clone, Copy, Eq, PartialEq)]
enum GuardianCheckpointOriginKindV1 {
    Genesis {
        spawn_effect_id: Uuid,
    },
    Record {
        durable_pane_id: Uuid,
        segment_id: Uuid,
        output_sequence: u64,
        output_record_digest: [u8; 32],
        output_committed_log_bytes: u64,
        journal_cumulative_plaintext_bytes: u64,
    },
}

/// Opaque stable origin of one durable checkpoint artifact.
///
/// Record origins identify an exact synchronized guardian-output record.
/// Genesis origins intentionally contain no pane identity: the exact Spawn
/// effect adopts the artifact before the child is admitted. Process-local
/// registration, upload, and lease-generation identities are excluded.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointOriginV1 {
    kind: GuardianCheckpointOriginKindV1,
}

impl GuardianCheckpointOriginV1 {
    /// Validate and construct a Genesis origin from the exact future Spawn
    /// effect identity carried on the wire.
    pub fn from_genesis_effect(
        spawn_effect_id: Uuid,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        if spawn_effect_id.is_nil() {
            return Err(GuardianCheckpointBoundaryError::NilGenesisEffectIdentity);
        }
        Ok(Self {
            kind: GuardianCheckpointOriginKindV1::Genesis { spawn_effect_id },
        })
    }

    /// Validate and construct a record origin from untrusted wire fields.
    #[allow(clippy::too_many_arguments)]
    pub fn from_record_parts(
        durable_pane_id: Uuid,
        segment_id: Uuid,
        output_sequence: u64,
        output_record_digest: [u8; 32],
        output_committed_log_bytes: u64,
        journal_cumulative_plaintext_bytes: u64,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        let origin = Self {
            kind: GuardianCheckpointOriginKindV1::Record {
                durable_pane_id,
                segment_id,
                output_sequence,
                output_record_digest,
                output_committed_log_bytes,
                journal_cumulative_plaintext_bytes,
            },
        };
        origin.validate()?;
        Ok(origin)
    }

    #[must_use]
    pub const fn is_genesis(&self) -> bool {
        matches!(self.kind, GuardianCheckpointOriginKindV1::Genesis { .. })
    }

    #[must_use]
    pub const fn spawn_effect_id(&self) -> Option<Uuid> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { spawn_effect_id } => {
                Some(spawn_effect_id)
            }
            GuardianCheckpointOriginKindV1::Record { .. } => None,
        }
    }

    #[must_use]
    pub const fn durable_pane_id(&self) -> Option<Uuid> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record {
                durable_pane_id, ..
            } => Some(durable_pane_id),
        }
    }

    #[must_use]
    pub const fn segment_id(&self) -> Option<Uuid> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record { segment_id, .. } => Some(segment_id),
        }
    }

    #[must_use]
    pub const fn output_sequence(&self) -> Option<u64> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record {
                output_sequence, ..
            } => Some(output_sequence),
        }
    }

    #[must_use]
    pub const fn output_record_digest(&self) -> Option<[u8; 32]> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record {
                output_record_digest,
                ..
            } => Some(output_record_digest),
        }
    }

    #[must_use]
    pub const fn output_committed_log_bytes(&self) -> Option<u64> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record {
                output_committed_log_bytes,
                ..
            } => Some(output_committed_log_bytes),
        }
    }

    #[must_use]
    pub const fn journal_cumulative_plaintext_bytes(&self) -> Option<u64> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => None,
            GuardianCheckpointOriginKindV1::Record {
                journal_cumulative_plaintext_bytes,
                ..
            } => Some(journal_cumulative_plaintext_bytes),
        }
    }

    fn validate(&self) -> Result<(), GuardianCheckpointBoundaryError> {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { spawn_effect_id } => {
                if spawn_effect_id.is_nil() {
                    return Err(GuardianCheckpointBoundaryError::NilGenesisEffectIdentity);
                }
            }
            GuardianCheckpointOriginKindV1::Record {
                durable_pane_id,
                segment_id,
                output_sequence,
                output_record_digest,
                output_committed_log_bytes,
                journal_cumulative_plaintext_bytes,
            } => {
                if durable_pane_id.is_nil() {
                    return Err(GuardianCheckpointBoundaryError::NilPaneIdentity);
                }
                if segment_id.is_nil() {
                    return Err(GuardianCheckpointBoundaryError::NilSegmentIdentity);
                }
                if output_sequence == 0 {
                    return Err(GuardianCheckpointBoundaryError::ZeroOutputSequence);
                }
                if output_record_digest == [0; 32] {
                    return Err(GuardianCheckpointBoundaryError::ZeroOutputRecordDigest);
                }
                if output_committed_log_bytes == 0 {
                    return Err(
                        GuardianCheckpointBoundaryError::ZeroOutputCommittedLogBytes,
                    );
                }
                if journal_cumulative_plaintext_bytes == 0 {
                    return Err(
                        GuardianCheckpointBoundaryError::ZeroJournalPlaintextWatermark,
                    );
                }
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for GuardianCheckpointOriginV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            GuardianCheckpointOriginKindV1::Genesis { spawn_effect_id } => formatter
                .debug_struct("GuardianCheckpointOriginV1::Genesis")
                .field("spawn_effect_id", &spawn_effect_id)
                .finish(),
            GuardianCheckpointOriginKindV1::Record {
                durable_pane_id,
                segment_id,
                output_sequence,
                output_committed_log_bytes,
                journal_cumulative_plaintext_bytes,
                ..
            } => formatter
                .debug_struct("GuardianCheckpointOriginV1::Record")
                .field("durable_pane_id", &durable_pane_id)
                .field("segment_id", &segment_id)
                .field("output_sequence", &output_sequence)
                .field("output_record_digest", &"[REDACTED]")
                .field(
                    "output_committed_log_bytes",
                    &output_committed_log_bytes,
                )
                .field(
                    "journal_cumulative_plaintext_bytes",
                    &journal_cumulative_plaintext_bytes,
                )
                .finish(),
        }
    }
}

/// Canonical metadata sufficient to recompute one stable checkpoint identity.
///
/// All fields are private so callers cannot manufacture an authority by
/// pairing terminal bytes with an unrelated journal or Spawn boundary.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointArtifactDescriptorV1 {
    origin: GuardianCheckpointOriginV1,
    parser_stream_bytes: u64,
    replay_identity_digest: [u8; 32],
    rows: u32,
    cols: u32,
    terminal_payload_bytes: u64,
    terminal_payload_digest: [u8; 32],
}

impl GuardianCheckpointArtifactDescriptorV1 {
    /// Construct the only record-backed production descriptor from the opaque
    /// live parser/output authority.
    pub fn from_live_capture(
        capture: &LiveParserCheckpointAck,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        let boundary = capture.boundary();
        let descriptor = Self::from_boundary(boundary);
        descriptor.validate_canonical_payload(
            capture.terminal_checkpoint().canonical_payload(),
            TerminalCheckpointLimits::default(),
        )?;
        if descriptor.recompute_boundary_identity_digest()?
            != capture.output_boundary_identity_digest()
            || descriptor.recompute_checkpoint_identity_digest()?
                != capture.checkpoint_artifact_identity_digest()
        {
            return Err(GuardianCheckpointBoundaryError::StableIdentityMismatch);
        }
        Ok(descriptor)
    }

    /// Construct a pre-spawn descriptor from a canonical terminal authority.
    /// The Spawn effect is the stable origin; pane assignment happens only
    /// when the guardian durably adopts this artifact.
    pub fn from_genesis_checkpoint(
        spawn_effect_id: Uuid,
        terminal_checkpoint: &RecoveryTerminalCheckpointV2,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        if terminal_checkpoint.parser_stream_bytes() != 0 {
            return Err(GuardianCheckpointBoundaryError::GenesisParserWatermark);
        }
        let rows = u32::try_from(terminal_checkpoint.rows())
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        let cols = u32::try_from(terminal_checkpoint.cols())
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        let (terminal_payload_bytes, terminal_payload_digest) =
            terminal_payload_identity(terminal_checkpoint.canonical_payload())?;
        let descriptor = Self {
            origin: GuardianCheckpointOriginV1::from_genesis_effect(spawn_effect_id)?,
            parser_stream_bytes: 0,
            replay_identity_digest: current_replay_identity_digest(),
            rows,
            cols,
            terminal_payload_bytes,
            terminal_payload_digest,
        };
        descriptor.validate_canonical_payload(
            terminal_checkpoint.canonical_payload(),
            TerminalCheckpointLimits::default(),
        )?;
        Ok(descriptor)
    }

    /// Validate untrusted wire fields and claimed stable identities before
    /// returning an opaque descriptor authority. The terminal payload itself
    /// must still pass [`Self::validate_canonical_payload`] before sealing.
    #[allow(clippy::too_many_arguments)]
    pub fn from_claimed_parts(
        claimed_boundary_identity_digest: [u8; 32],
        claimed_checkpoint_identity_digest: [u8; 32],
        origin: GuardianCheckpointOriginV1,
        parser_stream_bytes: u64,
        replay_identity_digest: [u8; 32],
        rows: u32,
        cols: u32,
        terminal_payload_bytes: u64,
        terminal_payload_digest: [u8; 32],
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        let descriptor = Self {
            origin,
            parser_stream_bytes,
            replay_identity_digest,
            rows,
            cols,
            terminal_payload_bytes,
            terminal_payload_digest,
        };
        descriptor.validate_identity_fields()?;
        if descriptor.replay_identity_digest != current_replay_identity_digest() {
            return Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch);
        }
        descriptor.validate_claimed_identity_digests(
            claimed_boundary_identity_digest,
            claimed_checkpoint_identity_digest,
        )?;
        Ok(descriptor)
    }

    /// Verify both raw identities claimed by a decoded wire descriptor.
    pub fn validate_claimed_identity_digests(
        &self,
        claimed_boundary_identity_digest: [u8; 32],
        claimed_checkpoint_identity_digest: [u8; 32],
    ) -> Result<(), GuardianCheckpointBoundaryError> {
        if self.recompute_boundary_identity_digest()? != claimed_boundary_identity_digest {
            return Err(GuardianCheckpointBoundaryError::ClaimedBoundaryIdentityMismatch);
        }
        if self.recompute_checkpoint_identity_digest()? != claimed_checkpoint_identity_digest {
            return Err(GuardianCheckpointBoundaryError::ClaimedCheckpointIdentityMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub const fn origin(&self) -> GuardianCheckpointOriginV1 {
        self.origin
    }

    #[must_use]
    pub const fn parser_stream_bytes(&self) -> u64 {
        self.parser_stream_bytes
    }

    #[must_use]
    pub const fn replay_identity_digest(&self) -> [u8; 32] {
        self.replay_identity_digest
    }

    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.cols
    }

    #[must_use]
    pub const fn terminal_payload_bytes(&self) -> u64 {
        self.terminal_payload_bytes
    }

    #[must_use]
    pub const fn terminal_payload_digest(&self) -> [u8; 32] {
        self.terminal_payload_digest
    }

    /// Recompute the stable boundary identity from every canonical preimage
    /// field. Registration, lease generation, and upload identity are absent.
    pub fn recompute_boundary_identity_digest(
        &self,
    ) -> Result<[u8; 32], GuardianCheckpointBoundaryError> {
        self.validate_identity_fields()?;
        Ok(self.canonical_boundary_identity_digest())
    }

    fn canonical_boundary_identity_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        match self.origin.kind {
            GuardianCheckpointOriginKindV1::Genesis { spawn_effect_id } => {
                hasher.update(GENESIS_BOUNDARY_IDENTITY_DIGEST_DOMAIN);
                hasher.update(spawn_effect_id.as_bytes());
            }
            GuardianCheckpointOriginKindV1::Record {
                durable_pane_id,
                segment_id,
                output_sequence,
                output_record_digest,
                output_committed_log_bytes,
                journal_cumulative_plaintext_bytes,
            } => {
                hasher.update(OUTPUT_BOUNDARY_IDENTITY_DIGEST_DOMAIN);
                hasher.update(GUARDIAN_CHECKPOINT_BOUNDARY_VERSION.to_le_bytes());
                hasher.update(durable_pane_id.as_bytes());
                hasher.update(segment_id.as_bytes());
                hasher.update(output_sequence.to_le_bytes());
                hasher.update(output_record_digest);
                hasher.update(output_committed_log_bytes.to_le_bytes());
                hasher.update(journal_cumulative_plaintext_bytes.to_le_bytes());
            }
        }
        hasher.finalize().into()
    }

    /// Recompute the stable complete artifact identity from the boundary and
    /// terminal/parser semantics.
    pub fn recompute_checkpoint_identity_digest(
        &self,
    ) -> Result<[u8; 32], GuardianCheckpointBoundaryError> {
        self.validate_identity_fields()?;
        Ok(self.canonical_checkpoint_identity_digest())
    }

    fn canonical_checkpoint_identity_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(CHECKPOINT_ARTIFACT_IDENTITY_DIGEST_DOMAIN);
        hasher.update(self.canonical_boundary_identity_digest());
        hasher.update(self.parser_stream_bytes.to_le_bytes());
        hasher.update(self.replay_identity_digest);
        hasher.update(self.rows.to_le_bytes());
        hasher.update(self.cols.to_le_bytes());
        hasher.update(self.terminal_payload_bytes.to_le_bytes());
        hasher.update(self.terminal_payload_digest);
        hasher.finalize().into()
    }

    /// Admit only bounded, byte-for-byte canonical terminal state matching the
    /// complete descriptor, including decoded geometry and current parser
    /// replay semantics.
    pub fn validate_canonical_payload(
        &self,
        canonical_terminal_payload: &[u8],
        limits: TerminalCheckpointLimits,
    ) -> Result<(), GuardianCheckpointBoundaryError> {
        self.validate_identity_fields()?;
        if self.replay_identity_digest != current_replay_identity_digest() {
            return Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch);
        }
        let validated = TerminalCheckpointV2::decode_canonical_json(
            canonical_terminal_payload,
            limits,
        )
        .map_err(|_| GuardianCheckpointBoundaryError::InvalidCanonicalTerminalPayload)?;
        if validated.rows() != self.rows || validated.cols() != self.cols {
            return Err(GuardianCheckpointBoundaryError::TerminalGeometryMismatch);
        }
        drop(validated);
        let (observed_payload_bytes, observed_payload_digest) =
            terminal_payload_identity(canonical_terminal_payload)?;
        if observed_payload_bytes != self.terminal_payload_bytes {
            return Err(GuardianCheckpointBoundaryError::TerminalPayloadLengthMismatch);
        }
        if observed_payload_digest != self.terminal_payload_digest {
            return Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch);
        }
        Ok(())
    }

    /// Bind a record-backed descriptor to the exact receipt reconstructed by
    /// guardian-owned output-journal recovery.
    pub fn validate_record_authority(
        &self,
        verified_segment: GuardianOutputSegmentIdentity,
        verified_output: GuardianOutputAppendReceipt,
    ) -> Result<(), GuardianCheckpointBoundaryError> {
        self.validate_identity_fields()?;
        let GuardianCheckpointOriginKindV1::Record {
            durable_pane_id,
            segment_id,
            output_sequence,
            output_record_digest,
            output_committed_log_bytes,
            journal_cumulative_plaintext_bytes,
        } = self.origin.kind
        else {
            return Err(GuardianCheckpointBoundaryError::GenesisHasNoRecordAuthority);
        };
        validate_output_identity(durable_pane_id, verified_segment, verified_output)?;
        if segment_id != verified_segment.segment_id()
            || output_sequence != verified_output.sequence()
            || output_record_digest != verified_output.record_digest()
            || output_committed_log_bytes != verified_output.committed_log_bytes()
            || journal_cumulative_plaintext_bytes
                != verified_output.cumulative_plaintext_bytes()
        {
            return Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch);
        }
        Ok(())
    }

    fn validate_identity_fields(&self) -> Result<(), GuardianCheckpointBoundaryError> {
        self.origin.validate()?;
        match self.origin.kind {
            GuardianCheckpointOriginKindV1::Genesis { .. } => {
                if self.parser_stream_bytes != 0 {
                    return Err(GuardianCheckpointBoundaryError::GenesisParserWatermark);
                }
            }
            GuardianCheckpointOriginKindV1::Record { .. } => {}
        }
        if self.replay_identity_digest == [0; 32] {
            return Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch);
        }
        if self.rows == 0 || self.cols == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroGeometry);
        }
        if self.terminal_payload_bytes == 0 {
            return Err(GuardianCheckpointBoundaryError::EmptyTerminalPayload);
        }
        if self.terminal_payload_digest == [0; 32] {
            return Err(GuardianCheckpointBoundaryError::ZeroTerminalPayloadDigest);
        }
        Ok(())
    }

    fn from_boundary(boundary: &GuardianCheckpointBoundary) -> Self {
        Self {
            origin: GuardianCheckpointOriginV1 {
                kind: GuardianCheckpointOriginKindV1::Record {
                    durable_pane_id: boundary.durable_pane_id(),
                    segment_id: boundary.segment_id(),
                    output_sequence: boundary.output_sequence(),
                    output_record_digest: boundary.output_record_digest(),
                    output_committed_log_bytes: boundary.output_committed_log_bytes(),
                    journal_cumulative_plaintext_bytes: boundary
                        .journal_cumulative_plaintext_bytes(),
                },
            },
            parser_stream_bytes: boundary.parser_stream_bytes(),
            replay_identity_digest: boundary.replay_identity_digest(),
            rows: boundary.rows(),
            cols: boundary.cols(),
            terminal_payload_bytes: boundary.terminal_payload_bytes(),
            terminal_payload_digest: boundary.terminal_payload_digest(),
        }
    }
}

impl std::fmt::Debug for GuardianCheckpointArtifactDescriptorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointArtifactDescriptorV1")
            .field("origin", &self.origin)
            .field("parser_stream_bytes", &self.parser_stream_bytes)
            .field("replay_identity_digest", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("terminal_payload_bytes", &self.terminal_payload_bytes)
            .field("terminal_payload_digest", &"[REDACTED]")
            .finish()
    }
}

/// Semantic role of one encrypted Phase-A checkpoint staging record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum GuardianCheckpointStageRecordKindV1 {
    CandidateMetadata = 1,
    Chunk = 2,
    SealManifest = 3,
}

impl GuardianCheckpointStageRecordKindV1 {
    fn from_wire(value: u8) -> Result<Self, GuardianCheckpointCipherError> {
        match value {
            1 => Ok(Self::CandidateMetadata),
            2 => Ok(Self::Chunk),
            3 => Ok(Self::SealManifest),
            _ => Err(GuardianCheckpointCipherError::InvalidRecordKind),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GuardianCheckpointStageScopeKindV1 {
    Pane {
        pane_id: Uuid,
        generation: u64,
    },
    Genesis {
        spawn_effect_id: Uuid,
    },
}

/// Exact lifetime scope of an encrypted Phase-A checkpoint upload.
///
/// A pane scope is fenced by its durable pane identity and lease generation.
/// A Genesis scope is instead fenced by the exact Spawn effect that must adopt
/// the artifact before the child is admitted.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointStageScopeV1 {
    kind: GuardianCheckpointStageScopeKindV1,
}

impl GuardianCheckpointStageScopeV1 {
    pub fn pane(pane_id: Uuid, generation: u64) -> Result<Self, GuardianCheckpointCipherError> {
        let scope = Self {
            kind: GuardianCheckpointStageScopeKindV1::Pane {
                pane_id,
                generation,
            },
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn genesis(spawn_effect_id: Uuid) -> Result<Self, GuardianCheckpointCipherError> {
        let scope = Self {
            kind: GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id },
        };
        scope.validate()?;
        Ok(scope)
    }

    #[must_use]
    pub const fn pane_identity(&self) -> Option<(Uuid, u64)> {
        match self.kind {
            GuardianCheckpointStageScopeKindV1::Pane {
                pane_id,
                generation,
            } => Some((pane_id, generation)),
            GuardianCheckpointStageScopeKindV1::Genesis { .. } => None,
        }
    }

    #[must_use]
    pub const fn spawn_effect_id(&self) -> Option<Uuid> {
        match self.kind {
            GuardianCheckpointStageScopeKindV1::Pane { .. } => None,
            GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id } => {
                Some(spawn_effect_id)
            }
        }
    }

    fn validate(&self) -> Result<(), GuardianCheckpointCipherError> {
        match self.kind {
            GuardianCheckpointStageScopeKindV1::Pane {
                pane_id,
                generation,
            } if !pane_id.is_nil() && generation > 0 => Ok(()),
            GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id }
                if !spawn_effect_id.is_nil() =>
            {
                Ok(())
            }
            _ => Err(GuardianCheckpointCipherError::InvalidScope),
        }
    }

    fn validate_descriptor(
        &self,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
    ) -> Result<(), GuardianCheckpointCipherError> {
        let matches = match (self.kind, descriptor.origin().kind) {
            (
                GuardianCheckpointStageScopeKindV1::Pane { pane_id, .. },
                GuardianCheckpointOriginKindV1::Record {
                    durable_pane_id, ..
                },
            ) => pane_id == durable_pane_id,
            (
                GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id },
                GuardianCheckpointOriginKindV1::Genesis {
                    spawn_effect_id: descriptor_effect,
                },
            ) => spawn_effect_id == descriptor_effect,
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(GuardianCheckpointCipherError::DescriptorScopeMismatch)
        }
    }
}

impl std::fmt::Debug for GuardianCheckpointStageScopeV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            GuardianCheckpointStageScopeKindV1::Pane {
                pane_id,
                generation,
            } => formatter
                .debug_struct("GuardianCheckpointStageScopeV1::Pane")
                .field("pane_id", &pane_id)
                .field("generation", &generation)
                .finish(),
            GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id } => formatter
                .debug_struct("GuardianCheckpointStageScopeV1::Genesis")
                .field("spawn_effect_id", &spawn_effect_id)
                .finish(),
        }
    }
}

/// Complete authenticated identity of one encrypted Phase-A staging record.
///
/// Private fields prevent unchecked construction. Production constructors
/// derive both stable checkpoint identities from the canonical descriptor and
/// derive the content identity from the exact plaintext. The persisted-parts
/// constructor validates an untrusted fixed header; successful opening still
/// requires a separately supplied exact expected context and re-identifies the
/// decrypted plaintext.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointStageRecordContextV1 {
    kind: GuardianCheckpointStageRecordKindV1,
    scope: GuardianCheckpointStageScopeV1,
    upload_id: Uuid,
    boundary_identity_digest: [u8; 32],
    checkpoint_identity_digest: [u8; 32],
    publication_id: Uuid,
    chunk_position: Option<(u32, u64)>,
    plaintext_bytes: u32,
    plaintext_digest: [u8; 32],
}

impl GuardianCheckpointStageRecordContextV1 {
    pub fn candidate_metadata(
        scope: GuardianCheckpointStageScopeV1,
        upload_id: Uuid,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
        publication_id: Uuid,
        plaintext: &[u8],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        Self::from_descriptor(
            GuardianCheckpointStageRecordKindV1::CandidateMetadata,
            scope,
            upload_id,
            descriptor,
            publication_id,
            None,
            plaintext,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn chunk(
        scope: GuardianCheckpointStageScopeV1,
        upload_id: Uuid,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
        publication_id: Uuid,
        index: u32,
        offset: u64,
        plaintext: &[u8],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        Self::from_descriptor(
            GuardianCheckpointStageRecordKindV1::Chunk,
            scope,
            upload_id,
            descriptor,
            publication_id,
            Some((index, offset)),
            plaintext,
        )
    }

    pub fn seal_manifest(
        scope: GuardianCheckpointStageScopeV1,
        upload_id: Uuid,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
        publication_id: Uuid,
        plaintext: &[u8],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        Self::from_descriptor(
            GuardianCheckpointStageRecordKindV1::SealManifest,
            scope,
            upload_id,
            descriptor,
            publication_id,
            None,
            plaintext,
        )
    }

    /// Reconstruct a context from a decoded private fixed header.
    ///
    /// Claimed identities remain untrusted until [`GuardianCheckpointCipher::open`]
    /// authenticates this exact context and re-identifies its plaintext.
    #[allow(clippy::too_many_arguments)]
    pub fn from_persisted_parts(
        kind: GuardianCheckpointStageRecordKindV1,
        scope: GuardianCheckpointStageScopeV1,
        upload_id: Uuid,
        boundary_identity_digest: [u8; 32],
        checkpoint_identity_digest: [u8; 32],
        publication_id: Uuid,
        chunk_position: Option<(u32, u64)>,
        plaintext_bytes: u32,
        plaintext_digest: [u8; 32],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        let context = Self {
            kind,
            scope,
            upload_id,
            boundary_identity_digest,
            checkpoint_identity_digest,
            publication_id,
            chunk_position,
            plaintext_bytes,
            plaintext_digest,
        };
        context.validate()?;
        Ok(context)
    }

    #[must_use]
    pub const fn kind(&self) -> GuardianCheckpointStageRecordKindV1 {
        self.kind
    }

    #[must_use]
    pub const fn scope(&self) -> GuardianCheckpointStageScopeV1 {
        self.scope
    }

    #[must_use]
    pub const fn upload_id(&self) -> Uuid {
        self.upload_id
    }

    #[must_use]
    pub const fn boundary_identity_digest(&self) -> [u8; 32] {
        self.boundary_identity_digest
    }

    #[must_use]
    pub const fn checkpoint_identity_digest(&self) -> [u8; 32] {
        self.checkpoint_identity_digest
    }

    #[must_use]
    pub const fn publication_id(&self) -> Uuid {
        self.publication_id
    }

    #[must_use]
    pub const fn chunk_position(&self) -> Option<(u32, u64)> {
        self.chunk_position
    }

    #[must_use]
    pub const fn plaintext_bytes(&self) -> u32 {
        self.plaintext_bytes
    }

    #[must_use]
    pub const fn plaintext_digest(&self) -> [u8; 32] {
        self.plaintext_digest
    }

    #[allow(clippy::too_many_arguments)]
    fn from_descriptor(
        kind: GuardianCheckpointStageRecordKindV1,
        scope: GuardianCheckpointStageScopeV1,
        upload_id: Uuid,
        descriptor: &GuardianCheckpointArtifactDescriptorV1,
        publication_id: Uuid,
        chunk_position: Option<(u32, u64)>,
        plaintext: &[u8],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        scope.validate_descriptor(descriptor)?;
        let boundary_identity_digest = descriptor
            .recompute_boundary_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)?;
        let checkpoint_identity_digest = descriptor
            .recompute_checkpoint_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)?;
        let (plaintext_bytes, plaintext_digest) = checkpoint_stage_plaintext_identity(plaintext)?;
        Self::from_persisted_parts(
            kind,
            scope,
            upload_id,
            boundary_identity_digest,
            checkpoint_identity_digest,
            publication_id,
            chunk_position,
            plaintext_bytes,
            plaintext_digest,
        )
    }

    fn validate(&self) -> Result<(), GuardianCheckpointCipherError> {
        self.scope.validate()?;
        if self.upload_id.is_nil() {
            return Err(GuardianCheckpointCipherError::NilUploadIdentity);
        }
        if self.publication_id.is_nil() {
            return Err(GuardianCheckpointCipherError::NilPublicationIdentity);
        }
        if self.boundary_identity_digest == [0; 32] {
            return Err(GuardianCheckpointCipherError::ZeroBoundaryIdentity);
        }
        if self.checkpoint_identity_digest == [0; 32] {
            return Err(GuardianCheckpointCipherError::ZeroCheckpointIdentity);
        }
        if self.plaintext_bytes == 0
            || self.plaintext_bytes > GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES
        {
            return Err(GuardianCheckpointCipherError::PlaintextByteLimit);
        }
        if self.plaintext_digest == [0; 32] {
            return Err(GuardianCheckpointCipherError::ZeroPlaintextDigest);
        }
        match (self.kind, self.chunk_position) {
            (GuardianCheckpointStageRecordKindV1::Chunk, Some((index, offset))) => {
                if index >= CHECKPOINT_STAGE_MAX_CHUNKS {
                    return Err(GuardianCheckpointCipherError::InvalidChunkIdentity);
                }
                let end = offset
                    .checked_add(u64::from(self.plaintext_bytes))
                    .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
                if end > CHECKPOINT_STAGE_MAX_ARTIFACT_BYTES {
                    return Err(GuardianCheckpointCipherError::InvalidChunkIdentity);
                }
            }
            (
                GuardianCheckpointStageRecordKindV1::CandidateMetadata
                | GuardianCheckpointStageRecordKindV1::SealManifest,
                None,
            ) => {}
            _ => return Err(GuardianCheckpointCipherError::InvalidChunkIdentity),
        }
        Ok(())
    }

    fn encode_canonical(&self) -> [u8; CHECKPOINT_STAGE_CONTEXT_BYTES] {
        let mut encoded = [0_u8; CHECKPOINT_STAGE_CONTEXT_BYTES];
        encoded[0] = self.kind as u8;
        match self.scope.kind {
            GuardianCheckpointStageScopeKindV1::Pane {
                pane_id,
                generation,
            } => {
                encoded[1] = 1;
                encoded[8..24].copy_from_slice(pane_id.as_bytes());
                encoded[24..32].copy_from_slice(&generation.to_le_bytes());
            }
            GuardianCheckpointStageScopeKindV1::Genesis { spawn_effect_id } => {
                encoded[1] = 2;
                encoded[8..24].copy_from_slice(spawn_effect_id.as_bytes());
            }
        }
        encoded[32..48].copy_from_slice(self.upload_id.as_bytes());
        encoded[48..80].copy_from_slice(&self.boundary_identity_digest);
        encoded[80..112].copy_from_slice(&self.checkpoint_identity_digest);
        encoded[112..128].copy_from_slice(self.publication_id.as_bytes());
        if let Some((index, offset)) = self.chunk_position {
            encoded[128..132].copy_from_slice(&index.to_le_bytes());
            encoded[136..144].copy_from_slice(&offset.to_le_bytes());
        }
        encoded[144..148].copy_from_slice(&self.plaintext_bytes.to_le_bytes());
        encoded[152..184].copy_from_slice(&self.plaintext_digest);
        encoded
    }

    fn decode_canonical(
        encoded: &[u8; CHECKPOINT_STAGE_CONTEXT_BYTES],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        if encoded[2..8] != [0; 6]
            || encoded[132..136] != [0; 4]
            || encoded[148..152] != [0; 4]
        {
            return Err(GuardianCheckpointCipherError::InvalidFixedHeader);
        }
        let kind = GuardianCheckpointStageRecordKindV1::from_wire(encoded[0])?;
        let identity = checkpoint_stage_uuid_at(encoded, 8);
        let generation = checkpoint_stage_u64_at(encoded, 24);
        let scope = match encoded[1] {
            1 => Self::decode_pane_scope(identity, generation)?,
            2 if generation == 0 => GuardianCheckpointStageScopeV1::genesis(identity)?,
            _ => return Err(GuardianCheckpointCipherError::InvalidScope),
        };
        let chunk_index = checkpoint_stage_u32_at(encoded, 128);
        let chunk_offset = checkpoint_stage_u64_at(encoded, 136);
        let chunk_position = match kind {
            GuardianCheckpointStageRecordKindV1::Chunk => Some((chunk_index, chunk_offset)),
            GuardianCheckpointStageRecordKindV1::CandidateMetadata
            | GuardianCheckpointStageRecordKindV1::SealManifest
                if chunk_index == 0 && chunk_offset == 0 =>
            {
                None
            }
            GuardianCheckpointStageRecordKindV1::CandidateMetadata
            | GuardianCheckpointStageRecordKindV1::SealManifest => {
                return Err(GuardianCheckpointCipherError::InvalidChunkIdentity);
            }
        };
        Self::from_persisted_parts(
            kind,
            scope,
            checkpoint_stage_uuid_at(encoded, 32),
            checkpoint_stage_digest_at(encoded, 48),
            checkpoint_stage_digest_at(encoded, 80),
            checkpoint_stage_uuid_at(encoded, 112),
            chunk_position,
            checkpoint_stage_u32_at(encoded, 144),
            checkpoint_stage_digest_at(encoded, 152),
        )
    }

    fn decode_pane_scope(
        pane_id: Uuid,
        generation: u64,
    ) -> Result<GuardianCheckpointStageScopeV1, GuardianCheckpointCipherError> {
        GuardianCheckpointStageScopeV1::pane(pane_id, generation)
    }
}

impl std::fmt::Debug for GuardianCheckpointStageRecordContextV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointStageRecordContextV1")
            .field("kind", &self.kind)
            .field("scope", &self.scope)
            .field("upload_id", &self.upload_id)
            .field("boundary_identity_digest", &"[REDACTED]")
            .field("checkpoint_identity_digest", &"[REDACTED]")
            .field("publication_id", &self.publication_id)
            .field("chunk_position", &self.chunk_position)
            .field("plaintext_bytes", &self.plaintext_bytes)
            .field("plaintext_digest", &"[REDACTED]")
            .finish()
    }
}

/// Checkpoint-only encryption authority backed by the guardian output key.
///
/// The API exposes neither key bytes, generic associated data, nor a sealing
/// operation with a caller-selected nonce. Every record is authenticated under
/// one typed, validated staging context and a fresh random nonce generated by
/// [`GuardianOutputCipher`].
#[derive(Clone)]
pub struct GuardianCheckpointCipher {
    output_cipher: GuardianOutputCipher,
}

impl GuardianCheckpointCipher {
    /// Clone the provisioned guardian cipher into a checkpoint-only authority.
    #[must_use]
    pub fn from_output_cipher(output_cipher: &GuardianOutputCipher) -> Self {
        Self {
            output_cipher: output_cipher.clone(),
        }
    }

    /// Return the nonsecret key fingerprint persisted in every record header.
    #[must_use]
    pub const fn key_id(&self) -> [u8; CHECKPOINT_STAGE_KEY_ID_BYTES] {
        self.output_cipher.key_id()
    }

    /// Seal one bounded plaintext under its exact typed staging context.
    pub fn seal(
        &self,
        context: GuardianCheckpointStageRecordContextV1,
        plaintext: &[u8],
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        context.validate()?;
        let (plaintext_bytes, plaintext_digest) = checkpoint_stage_plaintext_identity(plaintext)?;
        if plaintext_bytes != context.plaintext_bytes
            || !checkpoint_stage_digests_match(plaintext_digest, context.plaintext_digest)
        {
            return Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch);
        }
        let key_id = self.key_id();
        let aad = checkpoint_stage_record_aad(key_id, &context);
        let (nonce, ciphertext) = self
            .output_cipher
            .seal_guardian_metadata(plaintext, &aad)
            .map_err(|_| GuardianCheckpointCipherError::EncryptionFailed)?;
        let record = GuardianEncryptedCheckpointStageRecordV1 {
            version: GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION,
            key_id,
            nonce,
            context,
            ciphertext,
        };
        record.validate_bounded(GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES)?;
        Ok(record)
    }

    /// Authenticate and open one record only under the caller's exact expected
    /// context and an explicit per-call plaintext ceiling.
    pub fn open(
        &self,
        expected_context: &GuardianCheckpointStageRecordContextV1,
        record: &GuardianEncryptedCheckpointStageRecordV1,
        max_plaintext_bytes: u32,
    ) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointCipherError> {
        expected_context.validate()?;
        record.validate_bounded(max_plaintext_bytes)?;
        if !checkpoint_stage_key_ids_match(record.key_id, self.key_id()) {
            return Err(GuardianCheckpointCipherError::KeyIdentityMismatch);
        }
        if !checkpoint_stage_contexts_match(&record.context, expected_context) {
            return Err(GuardianCheckpointCipherError::ContextMismatch);
        }
        let aad = checkpoint_stage_record_aad(self.key_id(), expected_context);
        let plaintext = self
            .output_cipher
            .open_guardian_metadata(&record.nonce, &record.ciphertext, &aad)
            .map_err(|_| GuardianCheckpointCipherError::AuthenticationFailed)?;
        let (observed_bytes, observed_digest) =
            checkpoint_stage_plaintext_identity(plaintext.as_slice())?;
        if observed_bytes != expected_context.plaintext_bytes
            || !checkpoint_stage_digests_match(
                observed_digest,
                expected_context.plaintext_digest,
            )
        {
            return Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch);
        }
        Ok(plaintext)
    }
}

impl std::fmt::Debug for GuardianCheckpointCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointCipher")
            .field("key_id", &self.key_id())
            .finish_non_exhaustive()
    }
}

/// One authenticated encrypted Phase-A checkpoint record.
///
/// This envelope deliberately is not `Clone`. Its fixed header and ciphertext
/// may be persisted, but neither plaintext, nonce selection, nor generic AAD
/// are exposed through the public API.
pub struct GuardianEncryptedCheckpointStageRecordV1 {
    version: u32,
    key_id: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
    nonce: [u8; CHECKPOINT_STAGE_NONCE_BYTES],
    context: GuardianCheckpointStageRecordContextV1,
    ciphertext: Vec<u8>,
}

impl GuardianEncryptedCheckpointStageRecordV1 {
    /// Reconstruct and validate one private fixed-layout record from storage.
    pub fn from_persisted(
        header: &[u8],
        ciphertext: Vec<u8>,
        max_plaintext_bytes: u32,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        if header.len() != GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES
            || header[..8] != CHECKPOINT_STAGE_RECORD_MAGIC
            || header[12..16] != [0; 4]
        {
            return Err(GuardianCheckpointCipherError::InvalidFixedHeader);
        }
        let version = checkpoint_stage_u32_at(header, 8);
        if version != GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION {
            return Err(GuardianCheckpointCipherError::UnsupportedVersion { observed: version });
        }
        let mut key_id = [0_u8; CHECKPOINT_STAGE_KEY_ID_BYTES];
        key_id.copy_from_slice(&header[16..24]);
        if key_id == [0; CHECKPOINT_STAGE_KEY_ID_BYTES] {
            return Err(GuardianCheckpointCipherError::InvalidKeyIdentity);
        }
        let mut nonce = [0_u8; CHECKPOINT_STAGE_NONCE_BYTES];
        nonce.copy_from_slice(&header[24..48]);
        let mut encoded_context = [0_u8; CHECKPOINT_STAGE_CONTEXT_BYTES];
        encoded_context.copy_from_slice(&header[48..]);
        let context = GuardianCheckpointStageRecordContextV1::decode_canonical(&encoded_context)?;
        let record = Self {
            version,
            key_id,
            nonce,
            context,
            ciphertext,
        };
        record.validate_bounded(max_plaintext_bytes)?;
        Ok(record)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn key_id(&self) -> [u8; CHECKPOINT_STAGE_KEY_ID_BYTES] {
        self.key_id
    }

    #[must_use]
    pub const fn context(&self) -> GuardianCheckpointStageRecordContextV1 {
        self.context
    }

    #[must_use]
    pub const fn plaintext_bytes(&self) -> u32 {
        self.context.plaintext_bytes
    }

    #[must_use]
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    #[must_use]
    pub fn fixed_header(&self) -> [u8; GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES] {
        let mut header = [0_u8; GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES];
        header[..8].copy_from_slice(&CHECKPOINT_STAGE_RECORD_MAGIC);
        header[8..12].copy_from_slice(&self.version.to_le_bytes());
        header[16..24].copy_from_slice(&self.key_id);
        header[24..48].copy_from_slice(&self.nonce);
        header[48..].copy_from_slice(&self.context.encode_canonical());
        header
    }

    fn validate_bounded(
        &self,
        max_plaintext_bytes: u32,
    ) -> Result<(), GuardianCheckpointCipherError> {
        if self.version != GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION {
            return Err(GuardianCheckpointCipherError::UnsupportedVersion {
                observed: self.version,
            });
        }
        if self.key_id == [0; CHECKPOINT_STAGE_KEY_ID_BYTES] {
            return Err(GuardianCheckpointCipherError::InvalidKeyIdentity);
        }
        self.context.validate()?;
        if max_plaintext_bytes == 0
            || max_plaintext_bytes > GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES
        {
            return Err(GuardianCheckpointCipherError::InvalidCallerLimit);
        }
        if self.context.plaintext_bytes > max_plaintext_bytes {
            return Err(GuardianCheckpointCipherError::PlaintextByteLimit);
        }
        let plaintext_bytes = usize::try_from(self.context.plaintext_bytes)
            .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
        let expected_ciphertext_bytes = plaintext_bytes
            .checked_add(CHECKPOINT_STAGE_AEAD_TAG_BYTES)
            .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
        if self.ciphertext.len() != expected_ciphertext_bytes {
            return Err(GuardianCheckpointCipherError::CiphertextLengthMismatch);
        }
        Ok(())
    }
}

impl std::fmt::Debug for GuardianEncryptedCheckpointStageRecordV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianEncryptedCheckpointStageRecordV1")
            .field("version", &self.version)
            .field("key_id", &self.key_id)
            .field("nonce", &"[REDACTED]")
            .field("context", &self.context)
            .field("ciphertext_bytes", &self.ciphertext.len())
            .field("ciphertext", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum GuardianCheckpointCipherError {
    #[error("invalid checkpoint staging scope")]
    InvalidScope,
    #[error("checkpoint descriptor does not match its staging scope")]
    DescriptorScopeMismatch,
    #[error("checkpoint descriptor cannot mint stable staging identities")]
    InvalidDescriptor,
    #[error("checkpoint upload identity must be nonnil")]
    NilUploadIdentity,
    #[error("checkpoint publication identity must be nonnil")]
    NilPublicationIdentity,
    #[error("checkpoint staging boundary identity must be nonzero")]
    ZeroBoundaryIdentity,
    #[error("checkpoint staging artifact identity must be nonzero")]
    ZeroCheckpointIdentity,
    #[error("checkpoint staging plaintext digest must be nonzero")]
    ZeroPlaintextDigest,
    #[error("checkpoint staging record kind is invalid")]
    InvalidRecordKind,
    #[error("checkpoint staging chunk identity is invalid")]
    InvalidChunkIdentity,
    #[error("checkpoint staging plaintext exceeds its bound")]
    PlaintextByteLimit,
    #[error("checkpoint staging plaintext identity mismatch")]
    PlaintextIdentityMismatch,
    #[error("checkpoint staging caller limit is invalid")]
    InvalidCallerLimit,
    #[error("checkpoint staging fixed header is invalid")]
    InvalidFixedHeader,
    #[error("unsupported checkpoint staging record version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("checkpoint staging key identity is invalid")]
    InvalidKeyIdentity,
    #[error("checkpoint staging key identity mismatch")]
    KeyIdentityMismatch,
    #[error("checkpoint staging context mismatch")]
    ContextMismatch,
    #[error("checkpoint staging ciphertext length mismatch")]
    CiphertextLengthMismatch,
    #[error("checkpoint staging encryption failed")]
    EncryptionFailed,
    #[error("checkpoint staging authentication failed")]
    AuthenticationFailed,
    #[error("checkpoint staging arithmetic overflow")]
    ArithmeticOverflow,
}

fn checkpoint_stage_plaintext_identity(
    plaintext: &[u8],
) -> Result<(u32, [u8; 32]), GuardianCheckpointCipherError> {
    let plaintext_bytes = u32::try_from(plaintext.len())
        .map_err(|_| GuardianCheckpointCipherError::PlaintextByteLimit)?;
    if plaintext_bytes == 0
        || plaintext_bytes > GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES
    {
        return Err(GuardianCheckpointCipherError::PlaintextByteLimit);
    }
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_STAGE_PLAINTEXT_DIGEST_DOMAIN);
    hasher.update(plaintext_bytes.to_le_bytes());
    hasher.update(plaintext);
    Ok((plaintext_bytes, hasher.finalize().into()))
}

fn checkpoint_stage_record_aad(
    key_id: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
    context: &GuardianCheckpointStageRecordContextV1,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        CHECKPOINT_STAGE_RECORD_AEAD_DOMAIN.len()
            + std::mem::size_of::<u32>()
            + CHECKPOINT_STAGE_KEY_ID_BYTES
            + CHECKPOINT_STAGE_CONTEXT_BYTES,
    );
    aad.extend_from_slice(CHECKPOINT_STAGE_RECORD_AEAD_DOMAIN);
    aad.extend_from_slice(&GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION.to_le_bytes());
    aad.extend_from_slice(&key_id);
    aad.extend_from_slice(&context.encode_canonical());
    aad
}

fn checkpoint_stage_contexts_match(
    left: &GuardianCheckpointStageRecordContextV1,
    right: &GuardianCheckpointStageRecordContextV1,
) -> bool {
    checkpoint_stage_bytes_match(&left.encode_canonical(), &right.encode_canonical())
}

fn checkpoint_stage_key_ids_match(
    left: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
    right: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
) -> bool {
    checkpoint_stage_bytes_match(&left, &right)
}

fn checkpoint_stage_digests_match(left: [u8; 32], right: [u8; 32]) -> bool {
    checkpoint_stage_bytes_match(&left, &right)
}

fn checkpoint_stage_bytes_match(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

fn checkpoint_stage_uuid_at(bytes: &[u8], offset: usize) -> Uuid {
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&bytes[offset..offset + 16]);
    Uuid::from_bytes(encoded)
}

fn checkpoint_stage_digest_at(bytes: &[u8], offset: usize) -> [u8; 32] {
    let mut encoded = [0_u8; 32];
    encoded.copy_from_slice(&bytes[offset..offset + 32]);
    encoded
}

fn checkpoint_stage_u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut encoded = [0_u8; 4];
    encoded.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(encoded)
}

fn checkpoint_stage_u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut encoded = [0_u8; 8];
    encoded.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(encoded)
}

/// Exact synchronized raw-output position at which a terminal checkpoint was
/// captured while the parser was recovery-ground.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointBoundary {
    version: u32,
    durable_pane_id: Uuid,
    segment_id: Uuid,
    output_sequence: u64,
    output_record_digest: [u8; 32],
    output_committed_log_bytes: u64,
    journal_cumulative_plaintext_bytes: u64,
    parser_stream_bytes: u64,
    replay_identity_digest: [u8; 32],
    rows: u32,
    cols: u32,
    terminal_payload_bytes: u64,
    terminal_payload_digest: [u8; 32],
}

impl GuardianCheckpointBoundary {
    /// Capture a boundary from the exact output receipt already synchronized by
    /// the guardian and an opaque payload produced from the same terminal's own
    /// recovery-ground parser/model boundary. The terminal must include that
    /// record and no later record when this method is called.
    fn capture(
        expected_durable_pane_id: Uuid,
        segment: GuardianOutputSegmentIdentity,
        output: GuardianOutputAppendReceipt,
        terminal_checkpoint: &RecoveryTerminalCheckpointV2,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        validate_output_identity(expected_durable_pane_id, segment, output)?;
        let rows = u32::try_from(terminal_checkpoint.rows())
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        let cols = u32::try_from(terminal_checkpoint.cols())
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        if rows == 0 || cols == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroGeometry);
        }
        let (terminal_payload_bytes, terminal_payload_digest) =
            terminal_payload_identity(terminal_checkpoint.canonical_payload())?;

        Ok(Self {
            version: GUARDIAN_CHECKPOINT_BOUNDARY_VERSION,
            durable_pane_id: expected_durable_pane_id,
            segment_id: output.segment_id(),
            output_sequence: output.sequence(),
            output_record_digest: output.record_digest(),
            output_committed_log_bytes: output.committed_log_bytes(),
            journal_cumulative_plaintext_bytes: output.cumulative_plaintext_bytes(),
            parser_stream_bytes: terminal_checkpoint.parser_stream_bytes(),
            replay_identity_digest: current_replay_identity_digest(),
            rows,
            cols,
            terminal_payload_bytes,
            terminal_payload_digest,
        })
    }

    /// Validate a decoded boundary against the caller's exact pane authority,
    /// a record identity returned by verified output-journal recovery, and the
    /// already canonical-validated terminal payload before any state is
    /// admitted. A receipt from a different record, even in the same segment,
    /// cannot authorize this checkpoint.
    pub fn validate_for_restore(
        &self,
        expected_durable_pane_id: Uuid,
        verified_segment: GuardianOutputSegmentIdentity,
        verified_output: GuardianOutputAppendReceipt,
        canonical_terminal_payload: &[u8],
    ) -> Result<(), GuardianCheckpointBoundaryError> {
        if self.version != GUARDIAN_CHECKPOINT_BOUNDARY_VERSION {
            return Err(GuardianCheckpointBoundaryError::UnsupportedVersion {
                observed: self.version,
            });
        }
        if self.durable_pane_id.is_nil() {
            return Err(GuardianCheckpointBoundaryError::NilPaneIdentity);
        }
        if self.segment_id.is_nil() {
            return Err(GuardianCheckpointBoundaryError::NilSegmentIdentity);
        }
        if self.output_sequence == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroOutputSequence);
        }
        if self.output_committed_log_bytes == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroOutputCommittedLogBytes);
        }
        if self.journal_cumulative_plaintext_bytes == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroJournalPlaintextWatermark);
        }
        if self.rows == 0 || self.cols == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroGeometry);
        }
        if self.terminal_payload_bytes == 0 {
            return Err(GuardianCheckpointBoundaryError::EmptyTerminalPayload);
        }
        let expected = current_replay_identity_digest();
        if self.replay_identity_digest != expected {
            return Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch);
        }
        validate_output_identity(
            expected_durable_pane_id,
            verified_segment,
            verified_output,
        )?;
        if self.durable_pane_id != expected_durable_pane_id {
            return Err(GuardianCheckpointBoundaryError::ExpectedPaneIdentityMismatch);
        }
        if self.segment_id != verified_segment.segment_id()
            || self.output_sequence != verified_output.sequence()
            || self.output_record_digest != verified_output.record_digest()
            || self.output_committed_log_bytes != verified_output.committed_log_bytes()
            || self.journal_cumulative_plaintext_bytes
                != verified_output.cumulative_plaintext_bytes()
        {
            return Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch);
        }
        validate_terminal_payload_identity(self, canonical_terminal_payload)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn durable_pane_id(&self) -> Uuid {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn segment_id(&self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn output_sequence(&self) -> u64 {
        self.output_sequence
    }

    #[must_use]
    pub const fn output_record_digest(&self) -> [u8; 32] {
        self.output_record_digest
    }

    #[must_use]
    pub const fn output_committed_log_bytes(&self) -> u64 {
        self.output_committed_log_bytes
    }

    /// Authenticated pane-lifetime plaintext endpoint carried through segment
    /// rollover. This remains distinct from the parser-incarnation watermark.
    #[must_use]
    pub const fn journal_cumulative_plaintext_bytes(&self) -> u64 {
        self.journal_cumulative_plaintext_bytes
    }

    /// Cumulative bytes consumed by the one live parser incarnation.
    #[must_use]
    pub const fn parser_stream_bytes(&self) -> u64 {
        self.parser_stream_bytes
    }

    #[must_use]
    pub const fn replay_identity_digest(&self) -> [u8; 32] {
        self.replay_identity_digest
    }

    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.cols
    }

    #[must_use]
    pub const fn terminal_payload_bytes(&self) -> u64 {
        self.terminal_payload_bytes
    }

    #[must_use]
    pub const fn terminal_payload_digest(&self) -> [u8; 32] {
        self.terminal_payload_digest
    }

    /// Stable identity of the authenticated output-journal boundary covered by
    /// this checkpoint. Unlike the live-parser boundary digest, this excludes
    /// process-local registration identity and remains stable across a mux
    /// restart.
    #[must_use]
    pub fn output_boundary_identity_digest(&self) -> [u8; 32] {
        output_boundary_identity_digest(self)
    }

    /// Stable identity of the complete durable checkpoint artifact.
    ///
    /// The supplied canonical payload is re-identified before the digest is
    /// minted, so a caller cannot splice different terminal bytes onto this
    /// boundary. The result is suitable for
    /// `GuardianCheckpointIdentityDigest`; it deliberately excludes ephemeral
    /// live-registration identity.
    pub fn checkpoint_artifact_identity_digest(
        &self,
        canonical_terminal_payload: &[u8],
    ) -> Result<[u8; 32], GuardianCheckpointBoundaryError> {
        validate_terminal_payload_identity(self, canonical_terminal_payload)?;
        Ok(checkpoint_artifact_identity_digest(self))
    }
}

impl std::fmt::Debug for GuardianCheckpointBoundary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointBoundary")
            .field("version", &self.version)
            .field("durable_pane_id", &self.durable_pane_id)
            .field("segment_id", &self.segment_id)
            .field("output_sequence", &self.output_sequence)
            .field("output_record_digest", &"[REDACTED]")
            .field(
                "output_committed_log_bytes",
                &self.output_committed_log_bytes,
            )
            .field(
                "journal_cumulative_plaintext_bytes",
                &self.journal_cumulative_plaintext_bytes,
            )
            .field("parser_stream_bytes", &self.parser_stream_bytes)
            .field("replay_identity_digest", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("terminal_payload_bytes", &self.terminal_payload_bytes)
            .field("terminal_payload_digest", &"[REDACTED]")
            .finish()
    }
}

/// Non-constructible authority passed only by the live parser barrier while
/// its parser-ground witness and delivery fence are both held.
pub struct LiveParserCaptureAuthority {
    _private: (),
}

impl LiveParserCaptureAuthority {
    const fn issue() -> Self {
        Self { _private: () }
    }

    #[cfg(test)]
    pub(crate) const fn issue_for_test() -> Self {
        Self::issue()
    }
}

#[derive(Debug, Error)]
pub enum LiveParserPaneCaptureError {
    #[error("pane backend does not expose a live terminal checkpoint boundary")]
    Unsupported,
    #[error("terminal model checkpoint capture failed: {0}")]
    Terminal(#[source] RecoveryTerminalCheckpointError),
}

#[derive(Debug, Error)]
pub(crate) enum LiveParserCaptureAndBindError {
    #[error("live parser checkpoint registration identity changed before model capture")]
    RegistrationIdentityMismatch,
    #[error("pane terminal checkpoint callback failed")]
    Pane(#[source] LiveParserPaneCaptureError),
    #[error("pane checkpoint did not consume the exact parser action boundary")]
    PendingActionsRemain,
    #[error("live parser checkpoint identity binding failed")]
    Boundary(#[source] GuardianCheckpointBoundaryError),
}

/// Opaque authority minted only by the exact pending checkpoint state
/// transition. Callers cannot supply or replace any Ack identity component.
pub(crate) struct LiveParserCaptureRequest {
    request_id: u64,
    target: u64,
    durable_pane_id: Uuid,
    segment: GuardianOutputSegmentIdentity,
    output: GuardianOutputAppendReceipt,
    limits: frankenterm_term::terminalstate::checkpoint::TerminalCheckpointLimits,
    registration_wire_identity: [u8; 16],
    expected_pane: Weak<dyn Pane>,
    expected_generation: Weak<PaneRegistrationGeneration>,
}

impl LiveParserCaptureRequest {
    pub(super) const fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(super) const fn durable_pane_id(&self) -> Uuid {
        self.durable_pane_id
    }
}

impl LiveParserCheckpointControl {
    /// Atomically transition the one exact pending request into capture and
    /// mint its non-constructible binding authority. Identity fields are read
    /// only from registration-owned state; none are caller-provided here.
    pub(super) fn begin_capture(
        &self,
        target: u64,
    ) -> Result<Option<LiveParserCaptureRequest>, LiveParserCheckpointError> {
        let mut state = self.state.lock();
        let Some(pending) = state.pending.as_ref() else {
            return Ok(None);
        };
        if pending.cancelled {
            state.pending.take();
            drop(state);
            self.delivery_gate.notify_all();
            return Ok(None);
        }
        if pending.target != target
            || state.delivered_bytes != target
            || state.parsed_bytes != target
            || state.delivery_call_in_flight
            || state.socket_write_in_flight
        {
            return Err(LiveParserCheckpointError::ParserWatermarkMismatch);
        }
        if pending.capturing {
            return Ok(None);
        }
        let request = LiveParserCaptureRequest {
            request_id: pending.request_id,
            target,
            durable_pane_id: pending.durable_pane_id,
            segment: pending.segment,
            output: pending.output,
            limits: pending.limits,
            registration_wire_identity: state.registration_wire_identity,
            expected_pane: pending.expected_pane.clone(),
            expected_generation: pending.expected_generation.clone(),
        };
        state
            .pending
            .as_mut()
            .expect("pending checkpoint was just validated")
            .capturing = true;
        Ok(Some(request))
    }
}

/// Non-constructible publication authority for a checkpoint captured from the
/// reader-owned live parser.
///
/// All fields remain private so consumers cannot splice a terminal payload,
/// journal receipt, or registration generation. The registration wire identity
/// and [`Self::boundary_digest`] authenticate this live capture transaction;
/// they are ephemeral registration evidence, not a stable durable-checkpoint
/// identity and must not be persisted as one.
pub struct LiveParserCheckpointAck {
    registration_wire_identity: [u8; 16],
    boundary: GuardianCheckpointBoundary,
    boundary_digest: [u8; 32],
    terminal_checkpoint: RecoveryTerminalCheckpointV2,
}

#[allow(
    dead_code,
    reason = "artifact getters are the prepared guardian protocol publication seam"
)]
impl LiveParserCheckpointAck {
    fn capture(
        registration_wire_identity: [u8; 16],
        durable_pane_id: Uuid,
        segment: GuardianOutputSegmentIdentity,
        output: GuardianOutputAppendReceipt,
        target_parser_stream_bytes: u64,
        terminal_checkpoint: RecoveryTerminalCheckpointV2,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        if registration_wire_identity == [0; 16] {
            return Err(GuardianCheckpointBoundaryError::NilRegistrationWireIdentity);
        }
        if terminal_checkpoint.parser_stream_bytes() != target_parser_stream_bytes {
            return Err(GuardianCheckpointBoundaryError::ParserWatermarkMismatch);
        }
        let boundary = GuardianCheckpointBoundary::capture(
            durable_pane_id,
            segment,
            output,
            &terminal_checkpoint,
        )?;
        let boundary_digest = live_parser_boundary_digest(registration_wire_identity, &boundary);
        Ok(Self {
            registration_wire_identity,
            boundary,
            boundary_digest,
            terminal_checkpoint,
        })
    }

    pub const fn registration_wire_identity(&self) -> [u8; 16] {
        self.registration_wire_identity
    }

    pub const fn durable_pane_id(&self) -> Uuid {
        self.boundary.durable_pane_id()
    }

    pub const fn segment_id(&self) -> Uuid {
        self.boundary.segment_id()
    }

    pub const fn output_sequence(&self) -> u64 {
        self.boundary.output_sequence()
    }

    pub const fn output_record_digest(&self) -> [u8; 32] {
        self.boundary.output_record_digest()
    }

    pub const fn output_committed_log_bytes(&self) -> u64 {
        self.boundary.output_committed_log_bytes()
    }

    pub const fn journal_cumulative_plaintext_bytes(&self) -> u64 {
        self.boundary.journal_cumulative_plaintext_bytes()
    }

    pub const fn parser_stream_bytes(&self) -> u64 {
        self.boundary.parser_stream_bytes()
    }

    pub const fn terminal_payload_bytes(&self) -> u64 {
        self.boundary.terminal_payload_bytes()
    }

    pub const fn terminal_payload_digest(&self) -> [u8; 32] {
        self.boundary.terminal_payload_digest()
    }

    /// Stable identity of the authenticated output boundary included by this
    /// checkpoint. This remains stable across registration incarnations.
    #[must_use]
    pub fn output_boundary_identity_digest(&self) -> [u8; 32] {
        self.boundary.output_boundary_identity_digest()
    }

    /// Stable identity of the complete canonical checkpoint artifact.
    ///
    /// Capture already proved the payload-to-boundary binding, so this
    /// infallible accessor cannot be influenced by caller-supplied bytes.
    #[must_use]
    pub fn checkpoint_artifact_identity_digest(&self) -> [u8; 32] {
        checkpoint_artifact_identity_digest(&self.boundary)
    }

    /// Digest binding this boundary to the current registration wire identity.
    ///
    /// This value changes across registration incarnations and therefore must
    /// not be used as the identity of a serialized durable checkpoint.
    pub const fn boundary_digest(&self) -> [u8; 32] {
        self.boundary_digest
    }

    pub const fn boundary(&self) -> &GuardianCheckpointBoundary {
        &self.boundary
    }

    pub const fn terminal_checkpoint(&self) -> &RecoveryTerminalCheckpointV2 {
        &self.terminal_checkpoint
    }

    pub fn into_parts(
        self,
    ) -> (GuardianCheckpointBoundary, RecoveryTerminalCheckpointV2) {
        (self.boundary, self.terminal_checkpoint)
    }
}

impl std::fmt::Debug for LiveParserCheckpointAck {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveParserCheckpointAck")
            .field("registration_wire_identity", &"[REDACTED]")
            .field("boundary", &self.boundary)
            .field("boundary_digest", &"[REDACTED]")
            .field("terminal_checkpoint", &"[REDACTED]")
            .finish()
    }
}

/// Perform the only admissible live-parser model capture and ack construction
/// seam. The typed ground witness remains borrowed through action application,
/// model serialization, watermark comparison, and identity binding.
pub(crate) fn capture_and_bind_live_parser_checkpoint(
    pane: &Arc<dyn Pane>,
    capture_operation: &PaneRegistrationOperationLease,
    request: &LiveParserCaptureRequest,
    pending_actions: &mut Vec<Action>,
    ground: RecoveryGroundBoundary<'_>,
) -> Result<LiveParserCheckpointAck, LiveParserCaptureAndBindError> {
    let expected_pane = request
        .expected_pane
        .upgrade()
        .ok_or(LiveParserCaptureAndBindError::RegistrationIdentityMismatch)?;
    let expected_generation = request
        .expected_generation
        .upgrade()
        .ok_or(LiveParserCaptureAndBindError::RegistrationIdentityMismatch)?;
    let observed_durable_pane_id = pane.durable_pane_id().map(Uuid::from_bytes);
    if !Arc::ptr_eq(&expected_pane, pane)
        || !Arc::ptr_eq(&expected_generation, &capture_operation.generation)
        || expected_generation.wire_identity != request.registration_wire_identity
        || observed_durable_pane_id != Some(request.durable_pane_id)
    {
        return Err(LiveParserCaptureAndBindError::RegistrationIdentityMismatch);
    }
    if ground.stream_bytes() != request.target {
        return Err(LiveParserCaptureAndBindError::Boundary(
            GuardianCheckpointBoundaryError::ParserWatermarkMismatch,
        ));
    }
    let terminal_checkpoint = pane
        .capture_live_parser_checkpoint(
            LiveParserCaptureAuthority::issue(),
            pending_actions,
            ground,
            request.limits,
        )
        .map_err(LiveParserCaptureAndBindError::Pane)?;
    if !pending_actions.is_empty() || terminal_checkpoint.parser_stream_bytes() != request.target {
        return Err(LiveParserCaptureAndBindError::PendingActionsRemain);
    }
    LiveParserCheckpointAck::capture(
        request.registration_wire_identity,
        request.durable_pane_id,
        request.segment,
        request.output,
        request.target,
        terminal_checkpoint,
    )
    .map_err(LiveParserCaptureAndBindError::Boundary)
}

fn live_parser_boundary_digest(
    registration_wire_identity: [u8; 16],
    boundary: &GuardianCheckpointBoundary,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(LIVE_PARSER_BOUNDARY_DIGEST_DOMAIN);
    hasher.update(registration_wire_identity);
    hasher.update(boundary.version().to_le_bytes());
    hasher.update(boundary.durable_pane_id().as_bytes());
    hasher.update(boundary.segment_id().as_bytes());
    hasher.update(boundary.output_sequence().to_le_bytes());
    hasher.update(boundary.output_record_digest());
    hasher.update(boundary.output_committed_log_bytes().to_le_bytes());
    hasher.update(boundary.journal_cumulative_plaintext_bytes().to_le_bytes());
    hasher.update(boundary.parser_stream_bytes().to_le_bytes());
    hasher.update(boundary.replay_identity_digest());
    hasher.update(boundary.rows().to_le_bytes());
    hasher.update(boundary.cols().to_le_bytes());
    hasher.update(boundary.terminal_payload_bytes().to_le_bytes());
    hasher.update(boundary.terminal_payload_digest());
    hasher.finalize().into()
}

fn output_boundary_identity_digest(boundary: &GuardianCheckpointBoundary) -> [u8; 32] {
    GuardianCheckpointArtifactDescriptorV1::from_boundary(boundary)
        .canonical_boundary_identity_digest()
}

fn checkpoint_artifact_identity_digest(boundary: &GuardianCheckpointBoundary) -> [u8; 32] {
    GuardianCheckpointArtifactDescriptorV1::from_boundary(boundary)
        .canonical_checkpoint_identity_digest()
}

fn validate_terminal_payload_identity(
    boundary: &GuardianCheckpointBoundary,
    canonical_terminal_payload: &[u8],
) -> Result<(), GuardianCheckpointBoundaryError> {
    let (observed_payload_bytes, observed_payload_digest) =
        terminal_payload_identity(canonical_terminal_payload)?;
    if boundary.terminal_payload_bytes() != observed_payload_bytes {
        return Err(GuardianCheckpointBoundaryError::TerminalPayloadLengthMismatch);
    }
    if boundary.terminal_payload_digest() != observed_payload_digest {
        return Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch);
    }
    Ok(())
}

fn validate_output_identity(
    expected_durable_pane_id: Uuid,
    segment: GuardianOutputSegmentIdentity,
    output: GuardianOutputAppendReceipt,
) -> Result<(), GuardianCheckpointBoundaryError> {
    if expected_durable_pane_id.is_nil() {
        return Err(GuardianCheckpointBoundaryError::NilExpectedPaneIdentity);
    }
    if segment.durable_pane_id() != expected_durable_pane_id {
        return Err(GuardianCheckpointBoundaryError::ExpectedPaneIdentityMismatch);
    }
    if output.segment_id() != segment.segment_id() {
        return Err(GuardianCheckpointBoundaryError::OutputSegmentMismatch);
    }
    if output.sequence() < segment.first_sequence() {
        return Err(GuardianCheckpointBoundaryError::OutputBeforeSegment);
    }
    Ok(())
}

fn terminal_payload_identity(
    canonical_terminal_payload: &[u8],
) -> Result<(u64, [u8; 32]), GuardianCheckpointBoundaryError> {
    if canonical_terminal_payload.is_empty() {
        return Err(GuardianCheckpointBoundaryError::EmptyTerminalPayload);
    }
    let payload_bytes = u64::try_from(canonical_terminal_payload.len())
        .map_err(|_| GuardianCheckpointBoundaryError::TerminalPayloadLengthOutOfRange)?;
    let mut hasher = Sha256::new();
    hasher.update(TERMINAL_PAYLOAD_DIGEST_DOMAIN);
    hasher.update(payload_bytes.to_le_bytes());
    hasher.update(canonical_terminal_payload);
    Ok((payload_bytes, hasher.finalize().into()))
}

/// Current fixed parser compatibility identity, domain-separated for use in a
/// guardian checkpoint header.
#[must_use]
pub fn current_replay_identity_digest() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(REPLAY_IDENTITY_DIGEST_DOMAIN);
    hasher.update(RECOVERY_CHECKPOINT_PARSER_ID.as_bytes());
    hasher.update([0]);
    hasher.update(RECOVERY_TERMINAL_REPLAY_SEMANTICS_ID.as_bytes());
    hasher.finalize().into()
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum GuardianCheckpointBoundaryError {
    #[error("checkpoint expected pane identity must be nonnil")]
    NilExpectedPaneIdentity,
    #[error("checkpoint pane identity does not match the expected pane")]
    ExpectedPaneIdentityMismatch,
    #[error("checkpoint output receipt does not belong to the verified segment")]
    OutputSegmentMismatch,
    #[error("checkpoint output receipt precedes the verified segment")]
    OutputBeforeSegment,
    #[error("checkpoint geometry does not fit the v2 format")]
    GeometryOutOfRange,
    #[error("checkpoint geometry must have nonzero rows and columns")]
    ZeroGeometry,
    #[error("unsupported guardian checkpoint boundary version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("guardian checkpoint has a nil pane identity")]
    NilPaneIdentity,
    #[error("guardian checkpoint has a nil output segment identity")]
    NilSegmentIdentity,
    #[error("guardian checkpoint has a nil Genesis Spawn effect identity")]
    NilGenesisEffectIdentity,
    #[error("guardian checkpoint output sequence must be nonzero")]
    ZeroOutputSequence,
    #[error("guardian checkpoint output record digest must be nonzero")]
    ZeroOutputRecordDigest,
    #[error("guardian checkpoint output committed-log endpoint must be nonzero")]
    ZeroOutputCommittedLogBytes,
    #[error("guardian checkpoint journal plaintext watermark must be nonzero")]
    ZeroJournalPlaintextWatermark,
    #[error("live parser checkpoint registration wire identity must be nonnil")]
    NilRegistrationWireIdentity,
    #[error("live parser checkpoint terminal payload has the wrong parser watermark")]
    ParserWatermarkMismatch,
    #[error("Genesis checkpoint parser watermark must be zero")]
    GenesisParserWatermark,
    #[error("guardian checkpoint replay semantics identity is incompatible")]
    ReplayIdentityMismatch,
    #[error("guardian checkpoint terminal payload must be nonempty")]
    EmptyTerminalPayload,
    #[error("guardian checkpoint terminal payload length does not fit the v2 format")]
    TerminalPayloadLengthOutOfRange,
    #[error("guardian checkpoint terminal payload is not canonical valid state")]
    InvalidCanonicalTerminalPayload,
    #[error("guardian checkpoint terminal payload geometry does not match its descriptor")]
    TerminalGeometryMismatch,
    #[error("guardian checkpoint terminal payload digest must be nonzero")]
    ZeroTerminalPayloadDigest,
    #[error("guardian checkpoint stable identity disagrees with its capture authority")]
    StableIdentityMismatch,
    #[error("claimed guardian checkpoint boundary identity does not recompute")]
    ClaimedBoundaryIdentityMismatch,
    #[error("claimed guardian checkpoint artifact identity does not recompute")]
    ClaimedCheckpointIdentityMismatch,
    #[error("Genesis checkpoint has no guardian output-record authority")]
    GenesisHasNoRecordAuthority,
    #[error("guardian checkpoint does not match the verified output record")]
    VerifiedOutputIdentityMismatch,
    #[error("guardian checkpoint terminal payload length mismatch")]
    TerminalPayloadLengthMismatch,
    #[error("guardian checkpoint terminal payload digest mismatch")]
    TerminalPayloadDigestMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian_output_journal::{
        GuardianOutputCipher, GuardianOutputJournal, GuardianOutputJournalLimits,
    };
    use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointLimits;
    use frankenterm_term::{
        RecoveryTerminalCheckpointV2, Terminal, TerminalConfiguration, TerminalSize,
    };
    use std::fs::File;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct CheckpointTerminalConfig;

    impl TerminalConfiguration for CheckpointTerminalConfig {}

    fn terminal_checkpoint_with(
        rows: usize,
        cols: usize,
        term_version: &str,
    ) -> RecoveryTerminalCheckpointV2 {
        Terminal::new(
            TerminalSize {
                rows,
                cols,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(CheckpointTerminalConfig),
            "FrankenTerm",
            term_version,
            Box::new(Vec::<u8>::new()),
        )
        .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
        .expect("capture canonical terminal fixture")
    }

    fn terminal_checkpoint() -> RecoveryTerminalCheckpointV2 {
        terminal_checkpoint_with(24, 80, "guardian-checkpoint-test")
    }

    fn live_capture(
        registration_wire_identity: [u8; 16],
        pane: Uuid,
        segment: GuardianOutputSegmentIdentity,
        output: GuardianOutputAppendReceipt,
        checkpoint: RecoveryTerminalCheckpointV2,
    ) -> LiveParserCheckpointAck {
        let parser_stream_bytes = checkpoint.parser_stream_bytes();
        LiveParserCheckpointAck::capture(
            registration_wire_identity,
            pane,
            segment,
            output,
            parser_stream_bytes,
            checkpoint,
        )
        .expect("capture registration-bound checkpoint fixture")
    }

    fn record_descriptor() -> (
        GuardianCheckpointArtifactDescriptorV1,
        GuardianOutputSegmentIdentity,
        GuardianOutputAppendReceipt,
        LiveParserCheckpointAck,
    ) {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let capture = live_capture([1; 16], pane, segment, output, terminal_checkpoint());
        let descriptor = GuardianCheckpointArtifactDescriptorV1::from_live_capture(&capture)
            .expect("construct record descriptor");
        (descriptor, segment, output, capture)
    }

    fn synchronized_outputs(
        pane: Uuid,
        payloads: &[&[u8]],
    ) -> (
        GuardianOutputSegmentIdentity,
        Vec<GuardianOutputAppendReceipt>,
    ) {
        assert!(!payloads.is_empty(), "fixture requires at least one output");
        let directory = tempdir().expect("create temporary checkpoint directory");
        let path = directory.path().join("output.segment");
        let file = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("create output segment");
        let identity = GuardianOutputSegmentIdentity::new(pane, Uuid::new_v4(), 1, None)
            .expect("valid segment identity");
        let cipher = GuardianOutputCipher::try_from_key_slice(&[7_u8; 32])
            .expect("valid checkpoint test cipher");
        let mut journal = GuardianOutputJournal::open(
            file,
            identity,
            cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect("create output journal");
        let directory_file = File::open(directory.path()).expect("open parent directory");
        journal
            .sync_parent_directory_and_activate(&directory_file)
            .expect("activate output segment");
        let mut receipts = Vec::with_capacity(payloads.len());
        for payload in payloads {
            receipts.push(
                journal
                    .append_and_sync(*payload)
                    .expect("synchronize output record"),
            );
        }
        (identity, receipts)
    }

    fn synchronized_output(
        pane: Uuid,
    ) -> (
        GuardianOutputSegmentIdentity,
        GuardianOutputAppendReceipt,
    ) {
        let (identity, receipts) = synchronized_outputs(pane, &[b"checkpoint boundary"]);
        let receipt = receipts
            .into_iter()
            .next()
            .expect("fixture has one output receipt");
        (identity, receipt)
    }

    #[test]
    fn capture_binds_opaque_terminal_checkpoint_to_exact_synchronized_output() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();

        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture exact checkpoint boundary");

        assert_eq!(boundary.durable_pane_id(), pane);
        assert_eq!(boundary.segment_id(), output.segment_id());
        assert_eq!(boundary.output_sequence(), output.sequence());
        assert_eq!(boundary.output_record_digest(), output.record_digest());
        assert_eq!(
            boundary.replay_identity_digest(),
            current_replay_identity_digest()
        );
        let (payload_bytes, payload_digest) = terminal_payload_identity(
            checkpoint.canonical_payload(),
        )
        .expect("identify terminal payload");
        assert_eq!(boundary.terminal_payload_bytes(), payload_bytes);
        assert_eq!(boundary.terminal_payload_digest(), payload_digest);
        boundary
            .validate_for_restore(pane, segment, output, checkpoint.canonical_payload())
            .expect("current parser accepts its boundary");
    }

    #[test]
    fn unexpected_pane_cannot_publish_or_restore_a_boundary() {
        let pane = Uuid::new_v4();
        let other_pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();

        assert_eq!(
            GuardianCheckpointBoundary::capture(
                other_pane,
                segment,
                output,
                &checkpoint,
            ),
            Err(GuardianCheckpointBoundaryError::ExpectedPaneIdentityMismatch)
        );

        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture fixture boundary");
        assert_eq!(
            boundary.validate_for_restore(
                other_pane,
                segment,
                output,
                checkpoint.canonical_payload(),
            ),
            Err(GuardianCheckpointBoundaryError::ExpectedPaneIdentityMismatch)
        );
    }

    #[test]
    fn receipt_from_another_segment_cannot_publish_a_boundary() {
        let pane = Uuid::new_v4();
        let (segment, _) = synchronized_output(pane);
        let (_, other_output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();

        assert!(matches!(
            GuardianCheckpointBoundary::capture(
                pane,
                segment,
                other_output,
                &checkpoint,
            ),
            Err(GuardianCheckpointBoundaryError::OutputSegmentMismatch)
        ));
    }

    #[test]
    fn restore_requires_the_exact_verified_record_in_the_segment() {
        let pane = Uuid::new_v4();
        let (segment, receipts) =
            synchronized_outputs(pane, &[b"first output", b"second output"]);
        let first = receipts[0];
        let second = receipts[1];
        let checkpoint = terminal_checkpoint();
        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            first,
            &checkpoint,
        )
        .expect("capture first-record boundary");

        assert_eq!(
            boundary.validate_for_restore(
                pane,
                segment,
                second,
                checkpoint.canonical_payload(),
            ),
            Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch)
        );

        let mut digest_mutation = boundary;
        digest_mutation.output_record_digest[0] ^= 1;
        assert_eq!(
            digest_mutation.validate_for_restore(
                pane,
                segment,
                first,
                checkpoint.canonical_payload(),
            ),
            Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch)
        );
    }

    #[test]
    fn restore_rejects_terminal_payload_length_and_content_mutations() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();
        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture fixture boundary");

        let mut same_length_mutation = checkpoint.canonical_payload().to_vec();
        let final_byte = same_length_mutation
            .last_mut()
            .expect("fixture payload is nonempty");
        *final_byte ^= 1;
        assert_eq!(
            boundary.validate_for_restore(pane, segment, output, &same_length_mutation),
            Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch)
        );

        let mut longer_mutation = checkpoint.canonical_payload().to_vec();
        longer_mutation.push(b' ');
        assert_eq!(
            boundary.validate_for_restore(pane, segment, output, &longer_mutation),
            Err(GuardianCheckpointBoundaryError::TerminalPayloadLengthMismatch)
        );
    }

    #[test]
    fn restore_rejects_replay_semantics_identity_mutation() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();
        let mut boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture fixture boundary");
        boundary.replay_identity_digest[0] ^= 1;

        assert_eq!(
            boundary.validate_for_restore(
                pane,
                segment,
                output,
                checkpoint.canonical_payload(),
            ),
            Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch)
        );
    }

    #[test]
    fn durable_identities_exclude_registration_and_bind_checkpoint_semantics() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let first_checkpoint = terminal_checkpoint();
        let first_watermark = first_checkpoint.parser_stream_bytes();
        let first = LiveParserCheckpointAck::capture(
            [1_u8; 16],
            pane,
            segment,
            output,
            first_watermark,
            first_checkpoint,
        )
        .expect("capture first registration-bound checkpoint");
        let second_checkpoint = terminal_checkpoint();
        let second_watermark = second_checkpoint.parser_stream_bytes();
        let second = LiveParserCheckpointAck::capture(
            [2_u8; 16],
            pane,
            segment,
            output,
            second_watermark,
            second_checkpoint,
        )
        .expect("capture second registration-bound checkpoint");

        assert_ne!(first.boundary_digest(), second.boundary_digest());
        assert_eq!(
            first.output_boundary_identity_digest(),
            second.output_boundary_identity_digest(),
            "durable output identity must exclude process-local registration identity"
        );
        assert_eq!(
            first.checkpoint_artifact_identity_digest(),
            second.checkpoint_artifact_identity_digest(),
            "durable checkpoint identity must exclude process-local registration identity"
        );
        assert_ne!(first.output_boundary_identity_digest(), [0_u8; 32]);
        assert_ne!(first.checkpoint_artifact_identity_digest(), [0_u8; 32]);
        assert_eq!(
            first
                .boundary()
                .checkpoint_artifact_identity_digest(
                    first.terminal_checkpoint().canonical_payload(),
                )
                .expect("validate the captured canonical payload"),
            first.checkpoint_artifact_identity_digest()
        );

        let boundary = *first.boundary();
        let mut output_mutation = boundary;
        output_mutation.output_record_digest[0] ^= 1;
        assert_ne!(
            output_mutation.output_boundary_identity_digest(),
            boundary.output_boundary_identity_digest()
        );
        assert_ne!(
            checkpoint_artifact_identity_digest(&output_mutation),
            checkpoint_artifact_identity_digest(&boundary)
        );

        let mut parser_mutation = boundary;
        parser_mutation.parser_stream_bytes = parser_mutation
            .parser_stream_bytes
            .checked_add(1)
            .expect("fixture parser watermark has room");
        assert_eq!(
            parser_mutation.output_boundary_identity_digest(),
            boundary.output_boundary_identity_digest(),
            "parser state is not part of the durable journal boundary"
        );
        assert_ne!(
            checkpoint_artifact_identity_digest(&parser_mutation),
            checkpoint_artifact_identity_digest(&boundary)
        );

        let mut replay_mutation = boundary;
        replay_mutation.replay_identity_digest[0] ^= 1;
        assert_ne!(
            checkpoint_artifact_identity_digest(&replay_mutation),
            checkpoint_artifact_identity_digest(&boundary)
        );

        let mut payload_mutation = first.terminal_checkpoint().canonical_payload().to_vec();
        payload_mutation[0] ^= 1;
        assert_eq!(
            boundary.checkpoint_artifact_identity_digest(&payload_mutation),
            Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch)
        );
    }

    #[test]
    fn canonical_record_descriptor_matches_the_existing_live_identity() {
        let (descriptor, segment, output, capture) = record_descriptor();
        let origin = descriptor.origin();

        assert!(!origin.is_genesis());
        assert_eq!(origin.spawn_effect_id(), None);
        assert_eq!(origin.durable_pane_id(), Some(capture.durable_pane_id()));
        assert_eq!(origin.segment_id(), Some(capture.segment_id()));
        assert_eq!(origin.output_sequence(), Some(capture.output_sequence()));
        assert_eq!(
            origin.output_record_digest(),
            Some(capture.output_record_digest())
        );
        assert_eq!(
            origin.output_committed_log_bytes(),
            Some(capture.output_committed_log_bytes())
        );
        assert_eq!(
            origin.journal_cumulative_plaintext_bytes(),
            Some(capture.journal_cumulative_plaintext_bytes())
        );
        let mut expected_boundary = Sha256::new();
        expected_boundary.update(OUTPUT_BOUNDARY_IDENTITY_DIGEST_DOMAIN);
        expected_boundary.update(GUARDIAN_CHECKPOINT_BOUNDARY_VERSION.to_le_bytes());
        expected_boundary.update(capture.durable_pane_id().as_bytes());
        expected_boundary.update(capture.segment_id().as_bytes());
        expected_boundary.update(capture.output_sequence().to_le_bytes());
        expected_boundary.update(capture.output_record_digest());
        expected_boundary.update(capture.output_committed_log_bytes().to_le_bytes());
        expected_boundary.update(
            capture
                .journal_cumulative_plaintext_bytes()
                .to_le_bytes(),
        );
        let expected_boundary: [u8; 32] = expected_boundary.finalize().into();
        assert_eq!(
            descriptor
                .recompute_boundary_identity_digest()
                .expect("recompute record boundary"),
            expected_boundary
        );
        let mut expected_checkpoint = Sha256::new();
        expected_checkpoint.update(CHECKPOINT_ARTIFACT_IDENTITY_DIGEST_DOMAIN);
        expected_checkpoint.update(expected_boundary);
        expected_checkpoint.update(capture.parser_stream_bytes().to_le_bytes());
        expected_checkpoint.update(descriptor.replay_identity_digest());
        expected_checkpoint.update(descriptor.rows().to_le_bytes());
        expected_checkpoint.update(descriptor.cols().to_le_bytes());
        expected_checkpoint.update(descriptor.terminal_payload_bytes().to_le_bytes());
        expected_checkpoint.update(descriptor.terminal_payload_digest());
        let expected_checkpoint: [u8; 32] = expected_checkpoint.finalize().into();
        assert_eq!(
            descriptor
                .recompute_checkpoint_identity_digest()
                .expect("recompute record artifact"),
            expected_checkpoint
        );
        assert_eq!(capture.output_boundary_identity_digest(), expected_boundary);
        assert_eq!(
            capture.checkpoint_artifact_identity_digest(),
            expected_checkpoint
        );
        descriptor
            .validate_record_authority(segment, output)
            .expect("accept exact verified output receipt");
    }

    #[test]
    fn claimed_wire_parts_must_recompute_both_stable_identities() {
        let (descriptor, _, _, _) = record_descriptor();
        let boundary_identity = descriptor
            .recompute_boundary_identity_digest()
            .expect("baseline boundary identity");
        let checkpoint_identity = descriptor
            .recompute_checkpoint_identity_digest()
            .expect("baseline checkpoint identity");
        let reconstructed = GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
            boundary_identity,
            checkpoint_identity,
            descriptor.origin(),
            descriptor.parser_stream_bytes(),
            descriptor.replay_identity_digest(),
            descriptor.rows(),
            descriptor.cols(),
            descriptor.terminal_payload_bytes(),
            descriptor.terminal_payload_digest(),
        )
        .expect("accept exact claimed wire parts");
        assert_eq!(reconstructed, descriptor);

        let mut wrong_boundary = boundary_identity;
        wrong_boundary[0] ^= 1;
        assert_eq!(
            GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
                wrong_boundary,
                checkpoint_identity,
                descriptor.origin(),
                descriptor.parser_stream_bytes(),
                descriptor.replay_identity_digest(),
                descriptor.rows(),
                descriptor.cols(),
                descriptor.terminal_payload_bytes(),
                descriptor.terminal_payload_digest(),
            ),
            Err(GuardianCheckpointBoundaryError::ClaimedBoundaryIdentityMismatch)
        );

        let mut wrong_checkpoint = checkpoint_identity;
        wrong_checkpoint[0] ^= 1;
        assert_eq!(
            GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
                boundary_identity,
                wrong_checkpoint,
                descriptor.origin(),
                descriptor.parser_stream_bytes(),
                descriptor.replay_identity_digest(),
                descriptor.rows(),
                descriptor.cols(),
                descriptor.terminal_payload_bytes(),
                descriptor.terminal_payload_digest(),
            ),
            Err(GuardianCheckpointBoundaryError::ClaimedCheckpointIdentityMismatch)
        );

        let origin = descriptor.origin();
        let changed_origin = GuardianCheckpointOriginV1::from_record_parts(
            origin.durable_pane_id().expect("record pane"),
            origin.segment_id().expect("record segment"),
            origin
                .output_sequence()
                .expect("record sequence")
                .checked_add(1)
                .expect("fixture sequence has mutation room"),
            origin.output_record_digest().expect("record digest"),
            origin
                .output_committed_log_bytes()
                .expect("record committed bytes"),
            origin
                .journal_cumulative_plaintext_bytes()
                .expect("record cumulative bytes"),
        )
        .expect("construct structurally valid mutated origin");
        assert_eq!(
            GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
                boundary_identity,
                checkpoint_identity,
                changed_origin,
                descriptor.parser_stream_bytes(),
                descriptor.replay_identity_digest(),
                descriptor.rows(),
                descriptor.cols(),
                descriptor.terminal_payload_bytes(),
                descriptor.terminal_payload_digest(),
            ),
            Err(GuardianCheckpointBoundaryError::ClaimedBoundaryIdentityMismatch)
        );
    }

    #[test]
    fn record_descriptor_hashes_every_boundary_and_artifact_preimage() {
        let (descriptor, _, _, _) = record_descriptor();
        let boundary_identity = descriptor
            .recompute_boundary_identity_digest()
            .expect("baseline boundary identity");
        let artifact_identity = descriptor
            .recompute_checkpoint_identity_digest()
            .expect("baseline artifact identity");

        let mut pane_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record {
            durable_pane_id, ..
        } = &mut pane_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        *durable_pane_id = Uuid::new_v4();

        let mut segment_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record { segment_id, .. } =
            &mut segment_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        *segment_id = Uuid::new_v4();

        let mut sequence_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record {
            output_sequence, ..
        } = &mut sequence_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        *output_sequence = output_sequence
            .checked_add(1)
            .expect("fixture sequence has mutation room");

        let mut record_digest_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record {
            output_record_digest,
            ..
        } = &mut record_digest_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        output_record_digest[0] ^= 1;

        let mut committed_bytes_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record {
            output_committed_log_bytes,
            ..
        } = &mut committed_bytes_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        *output_committed_log_bytes = output_committed_log_bytes
            .checked_add(1)
            .expect("fixture committed endpoint has mutation room");

        let mut cumulative_bytes_mutation = descriptor;
        let GuardianCheckpointOriginKindV1::Record {
            journal_cumulative_plaintext_bytes,
            ..
        } = &mut cumulative_bytes_mutation.origin.kind
        else {
            panic!("fixture descriptor must be record-backed");
        };
        *journal_cumulative_plaintext_bytes = journal_cumulative_plaintext_bytes
            .checked_add(1)
            .expect("fixture plaintext endpoint has mutation room");

        for (field, mutation) in [
            ("durable pane", pane_mutation),
            ("segment", segment_mutation),
            ("output sequence", sequence_mutation),
            ("output record digest", record_digest_mutation),
            ("committed log bytes", committed_bytes_mutation),
            ("journal plaintext bytes", cumulative_bytes_mutation),
        ] {
            assert_ne!(
                mutation
                    .recompute_boundary_identity_digest()
                    .expect("mutated record boundary remains structurally valid"),
                boundary_identity,
                "{field} must affect the record boundary identity"
            );
            assert_ne!(
                mutation
                    .recompute_checkpoint_identity_digest()
                    .expect("mutated record artifact remains structurally valid"),
                artifact_identity,
                "{field} must affect the complete artifact identity"
            );
        }

        let mut parser_mutation = descriptor;
        parser_mutation.parser_stream_bytes = parser_mutation
            .parser_stream_bytes
            .checked_add(1)
            .expect("fixture parser watermark has mutation room");

        let mut replay_mutation = descriptor;
        replay_mutation.replay_identity_digest[0] ^= 1;

        let mut rows_mutation = descriptor;
        rows_mutation.rows = rows_mutation
            .rows
            .checked_add(1)
            .expect("fixture rows have mutation room");

        let mut columns_mutation = descriptor;
        columns_mutation.cols = columns_mutation
            .cols
            .checked_add(1)
            .expect("fixture columns have mutation room");

        let mut payload_bytes_mutation = descriptor;
        payload_bytes_mutation.terminal_payload_bytes = payload_bytes_mutation
            .terminal_payload_bytes
            .checked_add(1)
            .expect("fixture payload length has mutation room");

        let mut payload_digest_mutation = descriptor;
        payload_digest_mutation.terminal_payload_digest[0] ^= 1;

        for (field, mutation) in [
            ("parser stream bytes", parser_mutation),
            ("replay identity", replay_mutation),
            ("rows", rows_mutation),
            ("columns", columns_mutation),
            ("terminal payload bytes", payload_bytes_mutation),
            ("terminal payload digest", payload_digest_mutation),
        ] {
            assert_eq!(
                mutation
                    .recompute_boundary_identity_digest()
                    .expect("artifact-only mutation preserves valid boundary"),
                boundary_identity,
                "{field} must not change the underlying output boundary"
            );
            assert_ne!(
                mutation
                    .recompute_checkpoint_identity_digest()
                    .expect("artifact-only mutation remains structurally valid"),
                artifact_identity,
                "{field} must affect the complete artifact identity"
            );
        }
    }

    #[test]
    fn genesis_identity_is_bound_to_the_exact_spawn_effect() {
        let terminal = terminal_checkpoint();
        let first_effect = Uuid::new_v4();
        let second_effect = Uuid::new_v4();
        let first = GuardianCheckpointArtifactDescriptorV1::from_genesis_checkpoint(
            first_effect,
            &terminal,
        )
        .expect("construct first Genesis descriptor");
        let second = GuardianCheckpointArtifactDescriptorV1::from_genesis_checkpoint(
            second_effect,
            &terminal,
        )
        .expect("construct second Genesis descriptor");

        assert!(first.origin().is_genesis());
        assert_eq!(first.origin().spawn_effect_id(), Some(first_effect));
        assert_eq!(first.origin().durable_pane_id(), None);
        assert_eq!(first.parser_stream_bytes(), 0);
        let first_boundary_identity = first
            .recompute_boundary_identity_digest()
            .expect("first Genesis boundary");
        let first_checkpoint_identity = first
            .recompute_checkpoint_identity_digest()
            .expect("first Genesis artifact");
        assert_eq!(
            GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
                first_boundary_identity,
                first_checkpoint_identity,
                GuardianCheckpointOriginV1::from_genesis_effect(first_effect)
                    .expect("construct claimed Genesis origin"),
                first.parser_stream_bytes(),
                first.replay_identity_digest(),
                first.rows(),
                first.cols(),
                first.terminal_payload_bytes(),
                first.terminal_payload_digest(),
            )
            .expect("accept exact claimed Genesis parts"),
            first
        );
        let mut expected = Sha256::new();
        expected.update(GENESIS_BOUNDARY_IDENTITY_DIGEST_DOMAIN);
        expected.update(first_effect.as_bytes());
        assert_eq!(
            first_boundary_identity,
            <[u8; 32]>::from(expected.finalize())
        );
        assert_ne!(
            first_boundary_identity,
            second
                .recompute_boundary_identity_digest()
                .expect("second Genesis boundary")
        );
        assert_ne!(
            first_checkpoint_identity,
            second
                .recompute_checkpoint_identity_digest()
                .expect("second Genesis artifact")
        );
        let (segment, receipt) = synchronized_output(Uuid::new_v4());
        assert_eq!(
            first.validate_record_authority(segment, receipt),
            Err(GuardianCheckpointBoundaryError::GenesisHasNoRecordAuthority)
        );
    }

    #[test]
    fn descriptor_identity_is_independent_of_live_registration() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let first = live_capture([1; 16], pane, segment, output, terminal_checkpoint());
        let second = live_capture([2; 16], pane, segment, output, terminal_checkpoint());
        let first_descriptor = GuardianCheckpointArtifactDescriptorV1::from_live_capture(&first)
            .expect("construct first descriptor");
        let second_descriptor = GuardianCheckpointArtifactDescriptorV1::from_live_capture(&second)
            .expect("construct second descriptor");

        assert_ne!(first.boundary_digest(), second.boundary_digest());
        assert_eq!(first_descriptor, second_descriptor);
        assert_eq!(
            first_descriptor
                .recompute_checkpoint_identity_digest()
                .expect("first stable identity"),
            second_descriptor
                .recompute_checkpoint_identity_digest()
                .expect("second stable identity")
        );
    }

    #[test]
    fn descriptor_rejects_unsupported_replay_identity() {
        let (mut descriptor, _, _, capture) = record_descriptor();
        descriptor.replay_identity_digest[0] ^= 1;
        let boundary_identity = descriptor
            .recompute_boundary_identity_digest()
            .expect("unsupported replay identity does not alter boundary structure");
        let checkpoint_identity = descriptor
            .recompute_checkpoint_identity_digest()
            .expect("unsupported replay identity remains content-addressable");

        assert_eq!(
            descriptor.validate_canonical_payload(
                capture.terminal_checkpoint().canonical_payload(),
                TerminalCheckpointLimits::default(),
            ),
            Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch)
        );
        assert_eq!(
            GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
                boundary_identity,
                checkpoint_identity,
                descriptor.origin(),
                descriptor.parser_stream_bytes(),
                descriptor.replay_identity_digest(),
                descriptor.rows(),
                descriptor.cols(),
                descriptor.terminal_payload_bytes(),
                descriptor.terminal_payload_digest(),
            ),
            Err(GuardianCheckpointBoundaryError::ReplayIdentityMismatch)
        );
    }

    #[test]
    fn descriptor_rejects_canonical_geometry_and_content_splices() {
        let (descriptor, _, _, capture) = record_descriptor();
        let mut geometry_splice = descriptor;
        geometry_splice.rows = geometry_splice
            .rows
            .checked_add(1)
            .expect("fixture rows have mutation room");
        assert_eq!(
            geometry_splice.validate_canonical_payload(
                capture.terminal_checkpoint().canonical_payload(),
                TerminalCheckpointLimits::default(),
            ),
            Err(GuardianCheckpointBoundaryError::TerminalGeometryMismatch)
        );

        let content_splice = terminal_checkpoint_with(24, 80, "guardian-checkpoint-tesz");
        assert_eq!(
            content_splice.canonical_payload().len(),
            capture.terminal_checkpoint().canonical_payload().len(),
            "content-splice fixture must isolate the payload digest"
        );
        assert_eq!(
            descriptor.validate_canonical_payload(
                content_splice.canonical_payload(),
                TerminalCheckpointLimits::default(),
            ),
            Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch)
        );
    }

    #[test]
    fn descriptor_requires_the_exact_guardian_verified_receipt() {
        let pane = Uuid::new_v4();
        let (segment, receipts) =
            synchronized_outputs(pane, &[b"first record", b"second record"]);
        let first = receipts[0];
        let second = receipts[1];
        let capture = live_capture([1; 16], pane, segment, first, terminal_checkpoint());
        let descriptor = GuardianCheckpointArtifactDescriptorV1::from_live_capture(&capture)
            .expect("construct first-record descriptor");

        descriptor
            .validate_record_authority(segment, first)
            .expect("accept exact verified receipt");
        assert_eq!(
            descriptor.validate_record_authority(segment, second),
            Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch)
        );
    }

    #[test]
    fn opaque_terminal_checkpoint_debug_is_content_free() {
        let checkpoint = terminal_checkpoint();
        let debug = format!("{checkpoint:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("guardian-checkpoint-test"));
    }

    #[test]
    fn debug_omits_content_derived_digests() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();
        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture boundary");
        let debug = format!("{boundary:?}");

        assert!(!debug.contains(&hex::encode(output.record_digest())));
        assert!(!debug.contains(&hex::encode(current_replay_identity_digest())));
        assert!(!debug.contains(&hex::encode(boundary.terminal_payload_digest())));
        assert!(!debug.contains("guardian-checkpoint-test"));
        assert_eq!(debug.matches("[REDACTED]").count(), 3);
    }

    #[test]
    fn payload_mismatch_error_is_content_free() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let checkpoint = terminal_checkpoint();
        let boundary = GuardianCheckpointBoundary::capture(
            pane,
            segment,
            output,
            &checkpoint,
        )
        .expect("capture boundary");
        let mut mutation = checkpoint.canonical_payload().to_vec();
        mutation[0] ^= 1;
        let error = boundary
            .validate_for_restore(pane, segment, output, &mutation)
            .expect_err("mutated payload must fail");
        let diagnostic = format!("{error:?}: {error}");

        assert!(!diagnostic.contains("guardian-checkpoint-test"));
        assert!(!diagnostic.contains(&hex::encode(boundary.terminal_payload_digest())));
    }
}
