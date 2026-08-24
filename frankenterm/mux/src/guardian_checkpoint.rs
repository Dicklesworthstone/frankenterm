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
use crate::guardian_protocol::{
    GuardianCheckpointScopeV1, GuardianCheckpointStageKindV1,
    GuardianCheckpointStageRequestV1,
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
use zeroize::{Zeroize, Zeroizing};

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
    b"frankenterm.guardian-checkpoint-phase-a-record.v3\0";
const CHECKPOINT_STAGE_PLAINTEXT_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-phase-a-plaintext.v1\0";
const CHECKPOINT_SEAL_MANIFEST_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-canonical-seal-manifest.v1\0";
const CHECKPOINT_SEAL_OPERATION_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-seal-operation.v1\0";
const CHECKPOINT_STAGE_RECORD_MAGIC: [u8; 8] = *b"FTGCPA03";
const CHECKPOINT_STAGE_INNER_TRAILER_MAGIC: [u8; 8] = *b"FTGCPI01";

/// Version of the encrypted Phase-A checkpoint staging-record format.
///
/// Versions 1 and 2 were source-visible with rejected sealing-authority
/// models. A version-3 magic, version, and AEAD domain ensure no record minted
/// from caller-selected final-manifest bytes can be adopted by this format.
pub const GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION: u32 = 3;
/// Exact fixed header size emitted beside one encrypted staging-record body.
pub const GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES: usize = 232;
/// Hard per-record plaintext admission bound shared with checkpoint uploads.
pub const GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES: u32 = 256 * 1024;
/// Exact canonical Seal-request bytes bound into the encrypted manifest.
pub const GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES: u32 = 336;
/// Exact canonical final-manifest bytes: Seal request plus two digests.
pub const GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES: u32 = 400;
/// Capture generation reserved for a pre-spawn Genesis checkpoint.
pub const GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION: u64 = 1;

const CHECKPOINT_STAGE_CONTEXT_BYTES: usize = 184;
const CHECKPOINT_STAGE_KEY_ID_BYTES: usize = 8;
const CHECKPOINT_STAGE_NONCE_BYTES: usize = 24;
const CHECKPOINT_STAGE_AEAD_TAG_BYTES: usize = 16;
const CHECKPOINT_STAGE_INNER_TRAILER_BYTES: usize = 48;
const CHECKPOINT_STAGE_INNER_TRAILER_VERSION: u32 = 1;
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
/// Private fields prevent unchecked construction, but this value remains only
/// validated identity data. In particular, [`Self::from_claimed_parts`] does
/// not prove that a parser/output boundary or Spawn effect actually occurred;
/// final publication requires a separate nonconstructible authority.
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
    /// returning descriptor identity data. The terminal payload itself must
    /// still pass [`Self::validate_canonical_payload`], and this claimed value
    /// cannot authorize final publication.
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

/// Nonconstructible permit retained by the guardian's Spawn transaction.
///
/// This module deliberately exposes no production constructor from a raw UUID:
/// an effect identifier is identity data, not evidence that the corresponding
/// Spawn was authenticated, retained, and fenced against reuse. Until the
/// guardian protocol hands this module its exact retained-effect permit,
/// Genesis final sealing remains unavailable rather than manufacturing trust.
pub struct GuardianCheckpointGenesisSpawnPermitV1 {
    spawn_effect_id: Uuid,
    _private: (),
}

impl GuardianCheckpointGenesisSpawnPermitV1 {
    #[cfg(test)]
    fn issue_for_test(spawn_effect_id: Uuid) -> Self {
        assert!(!spawn_effect_id.is_nil());
        Self {
            spawn_effect_id,
            _private: (),
        }
    }
}

impl std::fmt::Debug for GuardianCheckpointGenesisSpawnPermitV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointGenesisSpawnPermitV1")
            .field("spawn_effect_id", &self.spawn_effect_id)
            .finish_non_exhaustive()
    }
}

/// Opaque proof that Phase-A storage inspected one exact authenticated
/// candidate plus its complete ordered chunk set and derived the two manifest
/// component identities from those records.
///
/// There is intentionally no production constructor in this module. The
/// upcoming guardian-runtime/journal integration must mint this witness only
/// after authenticating the exact Seal request, fencing the live incarnation,
/// and inspecting the complete stored assembly. Until then final publication
/// is fail-closed. In particular, raw request fields or digest arrays cannot
/// construct this type.
#[must_use = "validated stage assembly authority must be consumed by final sealing"]
pub struct GuardianCheckpointValidatedStageAssemblyV1 {
    seal_request: GuardianCheckpointStageRequestV1,
    publication_id: Uuid,
    candidate_record_digest: Zeroizing<[u8; 32]>,
    ordered_chunk_set_digest: Zeroizing<[u8; 32]>,
    _private: (),
}

impl GuardianCheckpointValidatedStageAssemblyV1 {
    #[cfg(test)]
    fn issue_for_test(
        seal_request: GuardianCheckpointStageRequestV1,
        publication_id: Uuid,
        candidate_record_digest: [u8; 32],
        ordered_chunk_set_digest: [u8; 32],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        if seal_request.kind() != GuardianCheckpointStageKindV1::Seal
            || seal_request.upload_id().is_nil()
            || publication_id.is_nil()
            || candidate_record_digest == [0; 32]
            || ordered_chunk_set_digest == [0; 32]
            || seal_request
                .encode()
                .map_err(|_| GuardianCheckpointCipherError::InvalidSealRequest)?
                .len()
                != usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
                    .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?
        {
            return Err(GuardianCheckpointCipherError::InvalidSealRequest);
        }
        Ok(Self {
            seal_request,
            publication_id,
            candidate_record_digest: Zeroizing::new(candidate_record_digest),
            ordered_chunk_set_digest: Zeroizing::new(ordered_chunk_set_digest),
            _private: (),
        })
    }
}

impl std::fmt::Debug for GuardianCheckpointValidatedStageAssemblyV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointValidatedStageAssemblyV1")
            .field("seal_request", &"[REDACTED]")
            .field("publication_id", &self.publication_id)
            .field("candidate_record_digest", &"[REDACTED]")
            .field("ordered_chunk_set_digest", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Nonconstructible authority proving that one complete checkpoint payload is
/// eligible for final publication.
///
/// A live record-backed authority is derived from the complete opaque
/// [`LiveParserCheckpointAck`], never from a claimed descriptor, caller bytes,
/// or a receipt supplied beside those bytes. Genesis authority additionally
/// consumes the guardian's exact retained Spawn-effect permit. The authority
/// is intentionally neither `Clone` nor `Copy`. It is necessary but not
/// sufficient for final sealing: binding also consumes one independently
/// validated Phase-A assembly witness.
///
/// A future cross-process constructor may safely consume an opaque
/// guardian-runtime attestation that proves the same facts. Keeping that
/// extension on this private-field authority type avoids ever accepting raw
/// descriptor fields or a raw Spawn UUID as publication authority.
pub struct GuardianCheckpointValidatedManifestAuthorityV1 {
    binding: GuardianCheckpointStageBindingV1,
}

impl GuardianCheckpointValidatedManifestAuthorityV1 {
    /// Mint publication authority only when this descriptor exactly matches
    /// the payload, parser watermark, segment, and synchronized receipt already
    /// sealed inside one nonconstructible live capture acknowledgement.
    pub fn from_live_capture(
        binding: &GuardianCheckpointStageBindingV1,
        capture: LiveParserCheckpointAck,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        let captured_descriptor =
            GuardianCheckpointArtifactDescriptorV1::from_live_capture(&capture)?;
        if binding.descriptor != captured_descriptor {
            return Err(GuardianCheckpointBoundaryError::LiveCaptureAuthorityMismatch);
        }
        Ok(Self { binding: *binding })
    }

    /// Mint Genesis publication authority only from a canonical pre-spawn
    /// terminal checkpoint and the one retained Spawn-effect permit.
    pub fn from_genesis_spawn_permit(
        binding: &GuardianCheckpointStageBindingV1,
        permit: GuardianCheckpointGenesisSpawnPermitV1,
        terminal_checkpoint: &RecoveryTerminalCheckpointV2,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        let authoritative_descriptor =
            GuardianCheckpointArtifactDescriptorV1::from_genesis_checkpoint(
                permit.spawn_effect_id,
                terminal_checkpoint,
            )?;
        if !binding.descriptor.origin.is_genesis() {
            return Err(GuardianCheckpointBoundaryError::RecordHasNoGenesisAuthority);
        }
        if binding.descriptor.origin.spawn_effect_id() != Some(permit.spawn_effect_id) {
            return Err(GuardianCheckpointBoundaryError::GenesisEffectIdentityMismatch);
        }
        if binding.descriptor != authoritative_descriptor {
            return Err(GuardianCheckpointBoundaryError::GenesisCheckpointAuthorityMismatch);
        }
        Ok(Self { binding: *binding })
    }

    /// Consume this one publication authority and bind it to one exact
    /// canonical Seal operation. The returned retry is a separately bounded
    /// capability for this same operation; it cannot be reminted or retargeted.
    pub fn bind_seal_operation(
        self,
        assembly: GuardianCheckpointValidatedStageAssemblyV1,
    ) -> Result<GuardianCheckpointManifestSealCapabilitiesV1, GuardianCheckpointCipherError> {
        GuardianCheckpointManifestSealCapabilitiesV1::from_authority(self, assembly)
    }
}

impl std::fmt::Debug for GuardianCheckpointValidatedManifestAuthorityV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointValidatedManifestAuthorityV1")
            .field("binding", &"[REDACTED]")
            .finish_non_exhaustive()
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
                if !spawn_effect_id.is_nil() => Ok(()),
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

/// Opaque bridge from a protocol checkpoint descriptor and stage scope into
/// storage authority.
///
/// The protocol's capture generation is deliberately excluded from stable
/// checkpoint digests, but it must exactly match the live pane scope before
/// any bytes can be staged. Genesis is pinned to the one reserved generation
/// and the exact Spawn effect already carried by the canonical descriptor.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointStageBindingV1 {
    scope: GuardianCheckpointStageScopeV1,
    descriptor: GuardianCheckpointArtifactDescriptorV1,
}

impl GuardianCheckpointStageBindingV1 {
    pub fn from_protocol_capture(
        scope: GuardianCheckpointStageScopeV1,
        descriptor: GuardianCheckpointArtifactDescriptorV1,
        protocol_capture_generation: u64,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        scope.validate_descriptor(&descriptor)?;
        match scope.kind {
            GuardianCheckpointStageScopeKindV1::Pane { generation, .. }
                if generation == protocol_capture_generation
                    && protocol_capture_generation > 0 => {}
            GuardianCheckpointStageScopeKindV1::Pane { .. } => {
                return Err(GuardianCheckpointCipherError::CaptureGenerationMismatch);
            }
            GuardianCheckpointStageScopeKindV1::Genesis { .. }
                if protocol_capture_generation
                    == GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION => {}
            GuardianCheckpointStageScopeKindV1::Genesis { .. } => {
                return Err(GuardianCheckpointCipherError::GenesisCaptureGenerationMismatch);
            }
        }
        descriptor
            .recompute_boundary_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)?;
        descriptor
            .recompute_checkpoint_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)?;
        Ok(Self { scope, descriptor })
    }

    #[must_use]
    pub const fn scope(&self) -> GuardianCheckpointStageScopeV1 {
        self.scope
    }

    fn boundary_identity_digest(&self) -> Result<[u8; 32], GuardianCheckpointCipherError> {
        self.descriptor
            .recompute_boundary_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)
    }

    fn checkpoint_identity_digest(&self) -> Result<[u8; 32], GuardianCheckpointCipherError> {
        self.descriptor
            .recompute_checkpoint_identity_digest()
            .map_err(|_| GuardianCheckpointCipherError::InvalidDescriptor)
    }

    fn validate_seal_request(
        &self,
        request: &GuardianCheckpointStageRequestV1,
    ) -> Result<(), GuardianCheckpointCipherError> {
        if request.kind() != GuardianCheckpointStageKindV1::Seal {
            return Err(GuardianCheckpointCipherError::InvalidSealRequest);
        }
        let protocol_descriptor = request.descriptor();
        let canonical_descriptor = protocol_descriptor
            .canonical_descriptor()
            .map_err(|_| GuardianCheckpointCipherError::InvalidSealRequest)?;
        let protocol_scope = checkpoint_stage_scope_from_protocol(request.scope())?;
        let request_binding = Self::from_protocol_capture(
            protocol_scope,
            canonical_descriptor,
            protocol_descriptor.capture_generation(),
        )?;
        if request_binding == *self {
            Ok(())
        } else {
            Err(GuardianCheckpointCipherError::ManifestAuthorityMismatch)
        }
    }
}

impl std::fmt::Debug for GuardianCheckpointStageBindingV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointStageBindingV1")
            .field("scope", &self.scope)
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

/// Single-use authority and zeroizing plaintext for one Phase-A seal.
///
/// This type deliberately implements neither `Clone` nor `Copy`. Constructors
/// accept caller bytes only for Candidate Metadata and Chunk records. Final
/// Seal bytes can be created only by
/// [`GuardianCheckpointValidatedManifestAuthorityV1::bind_seal_operation`].
/// The digest remains in a zeroizing allocation until
/// [`GuardianCheckpointCipher::seal`] consumes the complete intent. No
/// plaintext or digest accessor is exposed.
#[must_use = "a checkpoint staging seal intent must be consumed by the checkpoint cipher"]
pub struct GuardianCheckpointStageSealIntentV1 {
    context: GuardianCheckpointStageRecordContextV1,
    expected_plaintext_digest: Zeroizing<[u8; 32]>,
    plaintext: Zeroizing<Vec<u8>>,
}

impl GuardianCheckpointStageSealIntentV1 {
    pub fn candidate_metadata(
        binding: &GuardianCheckpointStageBindingV1,
        upload_id: Uuid,
        publication_id: Uuid,
        plaintext: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        Self::from_binding(
            GuardianCheckpointStageRecordKindV1::CandidateMetadata,
            binding,
            upload_id,
            publication_id,
            None,
            plaintext,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn chunk(
        binding: &GuardianCheckpointStageBindingV1,
        upload_id: Uuid,
        publication_id: Uuid,
        index: u32,
        offset: u64,
        plaintext: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        Self::from_binding(
            GuardianCheckpointStageRecordKindV1::Chunk,
            binding,
            upload_id,
            publication_id,
            Some((index, offset)),
            plaintext,
        )
    }

    #[must_use]
    pub const fn context(&self) -> GuardianCheckpointStageRecordContextV1 {
        self.context
    }

    #[allow(clippy::too_many_arguments)]
    fn from_binding(
        kind: GuardianCheckpointStageRecordKindV1,
        binding: &GuardianCheckpointStageBindingV1,
        upload_id: Uuid,
        publication_id: Uuid,
        chunk_position: Option<(u32, u64)>,
        plaintext: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        if kind == GuardianCheckpointStageRecordKindV1::SealManifest {
            return Err(GuardianCheckpointCipherError::InvalidKindAuthority);
        }
        let boundary_identity_digest = binding.boundary_identity_digest()?;
        let checkpoint_identity_digest = binding.checkpoint_identity_digest()?;
        let (plaintext_bytes, plaintext_digest) =
            checkpoint_stage_plaintext_identity(plaintext.as_slice())?;
        if let Some((_, offset)) = chunk_position {
            let end = offset
                .checked_add(u64::from(plaintext_bytes))
                .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
            if end > binding.descriptor.terminal_payload_bytes() {
                return Err(GuardianCheckpointCipherError::InvalidChunkIdentity);
            }
        }
        let context = GuardianCheckpointStageRecordContextV1::from_persisted_parts(
            kind,
            binding.scope,
            upload_id,
            boundary_identity_digest,
            checkpoint_identity_digest,
            publication_id,
            chunk_position,
            plaintext_bytes,
        )?;
        Ok(Self {
            context,
            expected_plaintext_digest: plaintext_digest,
            plaintext,
        })
    }
}

impl std::fmt::Debug for GuardianCheckpointStageSealIntentV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointStageSealIntentV1")
            .field("context", &self.context)
            .field("expected_plaintext_digest", &"[REDACTED]")
            .field("plaintext", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// One exact, canonical final-manifest operation.
///
/// This capability is neither `Clone` nor `Copy`; all fields are private, and
/// the only production issuance path consumes a validated capture/Genesis
/// authority. Canonical manifest bytes and their identities remain zeroizing
/// and have no accessor. A future guardian-runtime attestation constructor can
/// issue this same private representation after checking its MAC-authenticated
/// Seal request and live mux/pane incarnation, without introducing any raw
/// request-field constructor here.
#[must_use = "a validated checkpoint manifest operation must be consumed"]
pub struct GuardianCheckpointValidatedManifestOperationV1 {
    binding: GuardianCheckpointStageBindingV1,
    context: GuardianCheckpointStageRecordContextV1,
    canonical_manifest: Zeroizing<Vec<u8>>,
    expected_plaintext_digest: Zeroizing<[u8; 32]>,
    expected_manifest_digest: Zeroizing<[u8; 32]>,
    expected_operation_digest: Zeroizing<[u8; 32]>,
}

impl GuardianCheckpointValidatedManifestOperationV1 {
    #[must_use]
    pub const fn context(&self) -> GuardianCheckpointStageRecordContextV1 {
        self.context
    }

    fn from_validated_parts(
        binding: GuardianCheckpointStageBindingV1,
        publication_id: Uuid,
        canonical_manifest: Zeroizing<Vec<u8>>,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        let seal_request = checkpoint_seal_request_from_manifest(&canonical_manifest)?;
        binding.validate_seal_request(&seal_request)?;
        checkpoint_validate_manifest_component_digests(&canonical_manifest)?;
        let context = GuardianCheckpointStageRecordContextV1::from_persisted_parts(
            GuardianCheckpointStageRecordKindV1::SealManifest,
            binding.scope,
            seal_request.upload_id(),
            binding.boundary_identity_digest()?,
            binding.checkpoint_identity_digest()?,
            publication_id,
            None,
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
        )?;
        let (_, expected_plaintext_digest) =
            checkpoint_stage_plaintext_identity(&canonical_manifest)?;
        let expected_manifest_digest = checkpoint_seal_manifest_identity(&canonical_manifest)?;
        let expected_operation_digest =
            checkpoint_seal_operation_identity(&context, &expected_manifest_digest);
        let operation = Self {
            binding,
            context,
            canonical_manifest,
            expected_plaintext_digest,
            expected_manifest_digest,
            expected_operation_digest,
        };
        operation.validate()?;
        Ok(operation)
    }

    fn validate(&self) -> Result<(), GuardianCheckpointCipherError> {
        self.context.validate()?;
        if self.context.kind != GuardianCheckpointStageRecordKindV1::SealManifest
            || self.context.plaintext_bytes != GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES
            || self.canonical_manifest.len()
                != usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
                    .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?
        {
            return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
        }
        let seal_request = checkpoint_seal_request_from_manifest(&self.canonical_manifest)?;
        self.binding.validate_seal_request(&seal_request)?;
        checkpoint_validate_manifest_component_digests(&self.canonical_manifest)?;
        let expected_context = GuardianCheckpointStageRecordContextV1::from_persisted_parts(
            GuardianCheckpointStageRecordKindV1::SealManifest,
            self.binding.scope,
            seal_request.upload_id(),
            self.binding.boundary_identity_digest()?,
            self.binding.checkpoint_identity_digest()?,
            self.context.publication_id,
            None,
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
        )?;
        if !self.context.same_wire_identity(&expected_context) {
            return Err(GuardianCheckpointCipherError::ManifestAuthorityMismatch);
        }
        let manifest_digest = checkpoint_seal_manifest_identity(&self.canonical_manifest)?;
        let (manifest_bytes, plaintext_digest) =
            checkpoint_stage_plaintext_identity(&self.canonical_manifest)?;
        if manifest_bytes != self.context.plaintext_bytes
            || !checkpoint_stage_digests_match(
                &plaintext_digest,
                &self.expected_plaintext_digest,
            )
        {
            return Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch);
        }
        if !checkpoint_stage_digests_match(
            &manifest_digest,
            &self.expected_manifest_digest,
        ) {
            return Err(GuardianCheckpointCipherError::SealManifestIdentityMismatch);
        }
        let operation_digest = checkpoint_seal_operation_identity(&self.context, &manifest_digest);
        if !checkpoint_stage_digests_match(
            &operation_digest,
            &self.expected_operation_digest,
        ) {
            return Err(GuardianCheckpointCipherError::SealOperationIdentityMismatch);
        }
        Ok(())
    }
}

impl std::fmt::Debug for GuardianCheckpointValidatedManifestOperationV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointValidatedManifestOperationV1")
            .field("context", &self.context)
            .field("binding", &"[REDACTED]")
            .field("canonical_manifest", &"[REDACTED]")
            .field("expected_plaintext_digest", &"[REDACTED]")
            .field("expected_manifest_digest", &"[REDACTED]")
            .field("expected_operation_digest", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Non-retargetable capability for bounded repeated attempts of the exact same
/// Seal operation.
///
/// The capability is neither `Clone` nor `Copy` and exposes no operation or
/// manifest bytes. Cipher retry methods borrow it, so transient read/fsync/ACK
/// failures do not consume the authority. The guardian worker/protocol owns
/// the retry/time budget. A process restart deliberately loses this in-memory
/// authority and remains fail-closed until the future authenticated
/// runtime/journal remint seam reconstructs the exact operation.
#[must_use = "a checkpoint manifest retry capability must be consumed or discarded"]
pub struct GuardianCheckpointManifestRetryCapabilityV1 {
    operation: GuardianCheckpointValidatedManifestOperationV1,
}

impl GuardianCheckpointManifestRetryCapabilityV1 {
    #[must_use]
    pub const fn context(&self) -> GuardianCheckpointStageRecordContextV1 {
        self.operation.context
    }

}

impl std::fmt::Debug for GuardianCheckpointManifestRetryCapabilityV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointManifestRetryCapabilityV1")
            .field("operation", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Exactly one primary Seal operation and one same-operation retry.
#[must_use = "checkpoint manifest capabilities must be consumed"]
pub struct GuardianCheckpointManifestSealCapabilitiesV1 {
    primary: GuardianCheckpointValidatedManifestOperationV1,
    retry: GuardianCheckpointManifestRetryCapabilityV1,
}

impl GuardianCheckpointManifestSealCapabilitiesV1 {
    fn from_authority(
        authority: GuardianCheckpointValidatedManifestAuthorityV1,
        assembly: GuardianCheckpointValidatedStageAssemblyV1,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        let GuardianCheckpointValidatedStageAssemblyV1 {
            seal_request,
            publication_id,
            candidate_record_digest,
            ordered_chunk_set_digest,
            _private: (),
        } = assembly;
        authority.binding.validate_seal_request(&seal_request)?;
        if seal_request.upload_id().is_nil() || publication_id.is_nil() {
            return Err(GuardianCheckpointCipherError::InvalidSealRequest);
        }
        if candidate_record_digest.iter().all(|byte| *byte == 0)
            || ordered_chunk_set_digest.iter().all(|byte| *byte == 0)
        {
            return Err(GuardianCheckpointCipherError::InvalidManifestComponentDigest);
        }
        let encoded_request = Zeroizing::new(
            seal_request
                .encode()
                .map_err(|_| GuardianCheckpointCipherError::InvalidSealRequest)?,
        );
        if encoded_request.len()
            != usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
                .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?
        {
            return Err(GuardianCheckpointCipherError::InvalidSealRequest);
        }
        let canonical_manifest = checkpoint_canonical_seal_manifest(
            &encoded_request,
            &candidate_record_digest,
            &ordered_chunk_set_digest,
        )?;
        let retry_manifest = checkpoint_zeroizing_copy(&canonical_manifest)?;
        let primary = GuardianCheckpointValidatedManifestOperationV1::from_validated_parts(
            authority.binding,
            publication_id,
            canonical_manifest,
        )?;
        let retry = GuardianCheckpointManifestRetryCapabilityV1 {
            operation: GuardianCheckpointValidatedManifestOperationV1::from_validated_parts(
                authority.binding,
                publication_id,
                retry_manifest,
            )?,
        };
        Ok(Self { primary, retry })
    }

    #[must_use]
    pub fn into_primary_and_retry(
        self,
    ) -> (
        GuardianCheckpointValidatedManifestOperationV1,
        GuardianCheckpointManifestRetryCapabilityV1,
    ) {
        (self.primary, self.retry)
    }
}

impl std::fmt::Debug for GuardianCheckpointManifestSealCapabilitiesV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianCheckpointManifestSealCapabilitiesV1")
            .field("primary", &"[REDACTED]")
            .field("retry", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

/// Persistable authenticated wire identity of one Phase-A staging record.
///
/// This copyable value deliberately contains neither plaintext-derived digest
/// nor sealing authority. It is always only a persisted wire claim, including
/// when returned by a freshly encrypted record. Only the non-copyable
/// [`GuardianCheckpointStageSealIntentV1`] can authorize sealing.
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
}

impl GuardianCheckpointStageRecordContextV1 {
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

    fn same_wire_identity(&self, other: &Self) -> bool {
        checkpoint_stage_bytes_match(&self.encode_canonical(), &other.encode_canonical())
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
        encoded
    }

    fn decode_canonical(
        encoded: &[u8; CHECKPOINT_STAGE_CONTEXT_BYTES],
    ) -> Result<Self, GuardianCheckpointCipherError> {
        if encoded[2..8] != [0; 6]
            || encoded[132..136] != [0; 4]
            || encoded[148..152] != [0; 4]
            || encoded[152..184] != [0; 32]
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

    /// Consume one bounded Candidate Metadata or Chunk staging intent.
    pub fn seal(
        &self,
        intent: GuardianCheckpointStageSealIntentV1,
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        let context = intent.context;
        if !matches!(
            context.kind,
            GuardianCheckpointStageRecordKindV1::CandidateMetadata
                | GuardianCheckpointStageRecordKindV1::Chunk
        ) {
            return Err(GuardianCheckpointCipherError::InvalidKindAuthority);
        }
        self.seal_exact_payload(
            context,
            intent.plaintext.as_slice(),
            &intent.expected_plaintext_digest,
        )
    }

    /// Consume one opaque, canonical, operation-bound final Seal capability.
    /// There is deliberately no final-manifest overload taking caller bytes.
    pub fn seal_manifest(
        &self,
        operation: GuardianCheckpointValidatedManifestOperationV1,
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        self.seal_validated_manifest(&operation)
    }

    /// Repeat only the exact operation frozen into a separately issued retry
    /// capability. The caller owns the bounded retry/time policy.
    pub fn retry_seal_manifest(
        &self,
        retry: &GuardianCheckpointManifestRetryCapabilityV1,
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        self.seal_validated_manifest(&retry.operation)
    }

    fn seal_validated_manifest(
        &self,
        operation: &GuardianCheckpointValidatedManifestOperationV1,
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        operation.validate()?;
        self.seal_exact_payload(
            operation.context,
            operation.canonical_manifest.as_slice(),
            &operation.expected_plaintext_digest,
        )
    }

    fn seal_exact_payload(
        &self,
        context: GuardianCheckpointStageRecordContextV1,
        plaintext: &[u8],
        expected_plaintext_digest: &[u8; 32],
    ) -> Result<GuardianEncryptedCheckpointStageRecordV1, GuardianCheckpointCipherError> {
        context.validate()?;
        let (plaintext_bytes, plaintext_digest) =
            checkpoint_stage_plaintext_identity(plaintext)?;
        if plaintext_bytes != context.plaintext_bytes
            || !checkpoint_stage_digests_match(
                &plaintext_digest,
                expected_plaintext_digest,
            )
        {
            return Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch);
        }
        let inner_plaintext = checkpoint_stage_inner_plaintext(plaintext, &plaintext_digest)?;
        let key_id = self.key_id();
        let aad = checkpoint_stage_record_aad(key_id, &context);
        let (nonce, ciphertext) = self
            .output_cipher
            .seal_guardian_metadata(inner_plaintext.as_slice(), &aad)
            .map_err(|_| GuardianCheckpointCipherError::EncryptionFailed)?;
        drop(inner_plaintext);
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

    /// Authenticate an existing final record and prove that its decrypted
    /// bytes are exactly the canonical manifest carried by this one operation.
    /// This is the adoption seam for the separately bounded retry capability.
    pub fn open_manifest(
        &self,
        operation: GuardianCheckpointValidatedManifestOperationV1,
        record: &GuardianEncryptedCheckpointStageRecordV1,
    ) -> Result<(), GuardianCheckpointCipherError> {
        self.open_validated_manifest(&operation, record)
    }

    /// Reconcile an existing final record under a reusable but immutable exact
    /// retry capability. No decrypted manifest bytes escape this method.
    pub fn retry_open_manifest(
        &self,
        retry: &GuardianCheckpointManifestRetryCapabilityV1,
        record: &GuardianEncryptedCheckpointStageRecordV1,
    ) -> Result<(), GuardianCheckpointCipherError> {
        self.open_validated_manifest(&retry.operation, record)
    }

    fn open_validated_manifest(
        &self,
        operation: &GuardianCheckpointValidatedManifestOperationV1,
        record: &GuardianEncryptedCheckpointStageRecordV1,
    ) -> Result<(), GuardianCheckpointCipherError> {
        operation.validate()?;
        let opened = self.open_exact_payload(
            &operation.context,
            record,
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
        )?;
        if !checkpoint_stage_bytes_match(&opened, &operation.canonical_manifest) {
            return Err(GuardianCheckpointCipherError::SealManifestIdentityMismatch);
        }
        Ok(())
    }

    /// Authenticate and open one record only under the caller's exact expected
    /// context and an explicit per-call plaintext ceiling.
    pub fn open(
        &self,
        expected_context: &GuardianCheckpointStageRecordContextV1,
        record: &GuardianEncryptedCheckpointStageRecordV1,
        max_plaintext_bytes: u32,
    ) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointCipherError> {
        if expected_context.kind == GuardianCheckpointStageRecordKindV1::SealManifest {
            return Err(GuardianCheckpointCipherError::InvalidKindAuthority);
        }
        self.open_exact_payload(expected_context, record, max_plaintext_bytes)
    }

    fn open_exact_payload(
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
        if !record.context.same_wire_identity(expected_context) {
            return Err(GuardianCheckpointCipherError::ContextMismatch);
        }
        let aad = checkpoint_stage_record_aad(self.key_id(), expected_context);
        let mut inner_plaintext = self
            .output_cipher
            .open_guardian_metadata(&record.nonce, &record.ciphertext, &aad)
            .map_err(|_| GuardianCheckpointCipherError::AuthenticationFailed)?;
        let plaintext_bytes = usize::try_from(expected_context.plaintext_bytes)
            .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
        let expected_inner_bytes = plaintext_bytes
            .checked_add(CHECKPOINT_STAGE_INNER_TRAILER_BYTES)
            .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
        if inner_plaintext.len() != expected_inner_bytes {
            return Err(GuardianCheckpointCipherError::InvalidInnerEnvelope);
        }
        let trailer = &inner_plaintext[plaintext_bytes..];
        if trailer[..8] != CHECKPOINT_STAGE_INNER_TRAILER_MAGIC
            || checkpoint_stage_u32_at(trailer, 8)
                != CHECKPOINT_STAGE_INNER_TRAILER_VERSION
            || trailer[12..16] != [0; 4]
        {
            return Err(GuardianCheckpointCipherError::InvalidInnerEnvelope);
        }
        let encrypted_plaintext_digest =
            Zeroizing::new(checkpoint_stage_digest_at(trailer, 16));
        let (observed_bytes, observed_digest) =
            checkpoint_stage_plaintext_identity(&inner_plaintext[..plaintext_bytes])?;
        if observed_bytes != expected_context.plaintext_bytes
            || !checkpoint_stage_digests_match(
                &observed_digest,
                &encrypted_plaintext_digest,
            )
        {
            return Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch);
        }
        inner_plaintext[plaintext_bytes..].zeroize();
        inner_plaintext.truncate(plaintext_bytes);
        Ok(inner_plaintext)
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

struct GuardianCheckpointStageDecodedHeaderV1 {
    version: u32,
    key_id: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
    nonce: [u8; CHECKPOINT_STAGE_NONCE_BYTES],
    context: GuardianCheckpointStageRecordContextV1,
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
    /// Validate only the fixed header and return the one exact bounded body
    /// length a storage reader may allocate and read next.
    pub fn persisted_ciphertext_bytes(
        header: &[u8],
        max_plaintext_bytes: u32,
    ) -> Result<usize, GuardianCheckpointCipherError> {
        let decoded = Self::decode_fixed_header(header)?;
        checkpoint_stage_expected_ciphertext_bytes(&decoded.context, max_plaintext_bytes)
    }

    /// Reconstruct and validate one private fixed-layout record from storage.
    pub fn from_persisted(
        header: &[u8],
        ciphertext: Vec<u8>,
        max_plaintext_bytes: u32,
    ) -> Result<Self, GuardianCheckpointCipherError> {
        let decoded = Self::decode_fixed_header(header)?;
        let expected_ciphertext_bytes =
            checkpoint_stage_expected_ciphertext_bytes(&decoded.context, max_plaintext_bytes)?;
        if ciphertext.len() != expected_ciphertext_bytes {
            return Err(GuardianCheckpointCipherError::CiphertextLengthMismatch);
        }
        Ok(Self {
            version: decoded.version,
            key_id: decoded.key_id,
            nonce: decoded.nonce,
            context: decoded.context,
            ciphertext,
        })
    }

    fn decode_fixed_header(
        header: &[u8],
    ) -> Result<GuardianCheckpointStageDecodedHeaderV1, GuardianCheckpointCipherError> {
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
        Ok(GuardianCheckpointStageDecodedHeaderV1 {
            version,
            key_id,
            nonce,
            context,
        })
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
    pub fn ciphertext_bytes(&self) -> usize {
        self.ciphertext.len()
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
        let expected_ciphertext_bytes =
            checkpoint_stage_expected_ciphertext_bytes(&self.context, max_plaintext_bytes)?;
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
    #[error("checkpoint pane scope does not match the protocol capture generation")]
    CaptureGenerationMismatch,
    #[error("Genesis checkpoint does not use the reserved capture generation")]
    GenesisCaptureGenerationMismatch,
    #[error("checkpoint manifest authority does not match its stage binding")]
    ManifestAuthorityMismatch,
    #[error("checkpoint final-manifest request is not one exact canonical Seal request")]
    InvalidSealRequest,
    #[error("checkpoint final-manifest component digest is invalid")]
    InvalidManifestComponentDigest,
    #[error("checkpoint final-manifest length is invalid")]
    InvalidSealManifestLength,
    #[error("checkpoint canonical final-manifest identity mismatch")]
    SealManifestIdentityMismatch,
    #[error("checkpoint final-manifest operation identity mismatch")]
    SealOperationIdentityMismatch,
    #[error("checkpoint upload identity must be nonnil")]
    NilUploadIdentity,
    #[error("checkpoint publication identity must be nonnil")]
    NilPublicationIdentity,
    #[error("checkpoint staging boundary identity must be nonzero")]
    ZeroBoundaryIdentity,
    #[error("checkpoint staging artifact identity must be nonzero")]
    ZeroCheckpointIdentity,
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
    #[error("checkpoint record kind lacks its required staging authority")]
    InvalidKindAuthority,
    #[error("checkpoint staging ciphertext length mismatch")]
    CiphertextLengthMismatch,
    #[error("checkpoint staging encryption failed")]
    EncryptionFailed,
    #[error("checkpoint staging authentication failed")]
    AuthenticationFailed,
    #[error("checkpoint encrypted inner envelope is invalid")]
    InvalidInnerEnvelope,
    #[error("checkpoint staging plaintext allocation failed")]
    PlaintextAllocationFailed,
    #[error("checkpoint staging arithmetic overflow")]
    ArithmeticOverflow,
}

fn checkpoint_stage_plaintext_identity(
    plaintext: &[u8],
) -> Result<(u32, Zeroizing<[u8; 32]>), GuardianCheckpointCipherError> {
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
    Ok((
        plaintext_bytes,
        Zeroizing::new(<[u8; 32]>::from(hasher.finalize())),
    ))
}

fn checkpoint_stage_scope_from_protocol(
    scope: GuardianCheckpointScopeV1,
) -> Result<GuardianCheckpointStageScopeV1, GuardianCheckpointCipherError> {
    match scope {
        GuardianCheckpointScopeV1::Pane {
            pane_id,
            generation,
        } => GuardianCheckpointStageScopeV1::pane(pane_id, generation),
        GuardianCheckpointScopeV1::Genesis { spawn_effect_id } => {
            GuardianCheckpointStageScopeV1::genesis(spawn_effect_id)
        }
    }
}

fn checkpoint_zeroizing_copy(
    source: &[u8],
) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointCipherError> {
    let mut copy = Zeroizing::new(Vec::new());
    copy.try_reserve_exact(source.len())
        .map_err(|_| GuardianCheckpointCipherError::PlaintextAllocationFailed)?;
    copy.extend_from_slice(source);
    Ok(copy)
}

fn checkpoint_canonical_seal_manifest(
    encoded_seal_request: &[u8],
    candidate_record_digest: &[u8; 32],
    ordered_chunk_set_digest: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointCipherError> {
    let request_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    let manifest_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    if encoded_seal_request.len() != request_bytes
        || candidate_record_digest.iter().all(|byte| *byte == 0)
        || ordered_chunk_set_digest.iter().all(|byte| *byte == 0)
        || request_bytes
            .checked_add(candidate_record_digest.len())
            .and_then(|bytes| bytes.checked_add(ordered_chunk_set_digest.len()))
            != Some(manifest_bytes)
    {
        return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
    }
    let mut manifest = Zeroizing::new(Vec::new());
    manifest
        .try_reserve_exact(manifest_bytes)
        .map_err(|_| GuardianCheckpointCipherError::PlaintextAllocationFailed)?;
    manifest.extend_from_slice(encoded_seal_request);
    manifest.extend_from_slice(candidate_record_digest);
    manifest.extend_from_slice(ordered_chunk_set_digest);
    if manifest.len() != manifest_bytes {
        return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
    }
    Ok(manifest)
}

fn checkpoint_seal_request_from_manifest(
    canonical_manifest: &[u8],
) -> Result<GuardianCheckpointStageRequestV1, GuardianCheckpointCipherError> {
    let request_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    let manifest_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    if canonical_manifest.len() != manifest_bytes {
        return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
    }
    let request = GuardianCheckpointStageRequestV1::decode(&canonical_manifest[..request_bytes])
        .map_err(|_| GuardianCheckpointCipherError::InvalidSealRequest)?;
    if request.kind() != GuardianCheckpointStageKindV1::Seal {
        return Err(GuardianCheckpointCipherError::InvalidSealRequest);
    }
    Ok(request)
}

fn checkpoint_validate_manifest_component_digests(
    canonical_manifest: &[u8],
) -> Result<(), GuardianCheckpointCipherError> {
    let request_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    let manifest_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?;
    if canonical_manifest.len() != manifest_bytes {
        return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
    }
    let candidate_end = request_bytes
        .checked_add(32)
        .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
    if canonical_manifest[request_bytes..candidate_end]
        .iter()
        .all(|byte| *byte == 0)
        || canonical_manifest[candidate_end..manifest_bytes]
            .iter()
            .all(|byte| *byte == 0)
    {
        return Err(GuardianCheckpointCipherError::InvalidManifestComponentDigest);
    }
    Ok(())
}

fn checkpoint_seal_manifest_identity(
    canonical_manifest: &[u8],
) -> Result<Zeroizing<[u8; 32]>, GuardianCheckpointCipherError> {
    if canonical_manifest.len()
        != usize::try_from(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES)
            .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?
    {
        return Err(GuardianCheckpointCipherError::InvalidSealManifestLength);
    }
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_SEAL_MANIFEST_DIGEST_DOMAIN);
    hasher.update(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES.to_le_bytes());
    hasher.update(canonical_manifest);
    Ok(Zeroizing::new(hasher.finalize().into()))
}

fn checkpoint_seal_operation_identity(
    context: &GuardianCheckpointStageRecordContextV1,
    manifest_digest: &[u8; 32],
) -> Zeroizing<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_SEAL_OPERATION_DIGEST_DOMAIN);
    hasher.update(GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION.to_le_bytes());
    hasher.update(context.encode_canonical());
    hasher.update(GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES.to_le_bytes());
    hasher.update(manifest_digest);
    Zeroizing::new(hasher.finalize().into())
}

fn checkpoint_stage_inner_plaintext(
    plaintext: &[u8],
    plaintext_digest: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, GuardianCheckpointCipherError> {
    let inner_bytes = plaintext
        .len()
        .checked_add(CHECKPOINT_STAGE_INNER_TRAILER_BYTES)
        .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?;
    let mut inner_plaintext = Zeroizing::new(Vec::new());
    inner_plaintext
        .try_reserve_exact(inner_bytes)
        .map_err(|_| GuardianCheckpointCipherError::PlaintextAllocationFailed)?;
    inner_plaintext.extend_from_slice(plaintext);
    inner_plaintext.extend_from_slice(&CHECKPOINT_STAGE_INNER_TRAILER_MAGIC);
    inner_plaintext.extend_from_slice(&CHECKPOINT_STAGE_INNER_TRAILER_VERSION.to_le_bytes());
    inner_plaintext.extend_from_slice(&[0; 4]);
    inner_plaintext.extend_from_slice(plaintext_digest);
    if inner_plaintext.len() != inner_bytes {
        return Err(GuardianCheckpointCipherError::ArithmeticOverflow);
    }
    Ok(inner_plaintext)
}

fn checkpoint_stage_expected_ciphertext_bytes(
    context: &GuardianCheckpointStageRecordContextV1,
    max_plaintext_bytes: u32,
) -> Result<usize, GuardianCheckpointCipherError> {
    context.validate()?;
    if max_plaintext_bytes == 0
        || max_plaintext_bytes > GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES
    {
        return Err(GuardianCheckpointCipherError::InvalidCallerLimit);
    }
    if context.plaintext_bytes > max_plaintext_bytes {
        return Err(GuardianCheckpointCipherError::PlaintextByteLimit);
    }
    usize::try_from(context.plaintext_bytes)
        .map_err(|_| GuardianCheckpointCipherError::ArithmeticOverflow)?
        .checked_add(CHECKPOINT_STAGE_INNER_TRAILER_BYTES)
        .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)?
        .checked_add(CHECKPOINT_STAGE_AEAD_TAG_BYTES)
        .ok_or(GuardianCheckpointCipherError::ArithmeticOverflow)
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

fn checkpoint_stage_key_ids_match(
    left: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
    right: [u8; CHECKPOINT_STAGE_KEY_ID_BYTES],
) -> bool {
    checkpoint_stage_bytes_match(&left, &right)
}

fn checkpoint_stage_digests_match(left: &[u8; 32], right: &[u8; 32]) -> bool {
    checkpoint_stage_bytes_match(left, right)
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
    #[error("claimed checkpoint descriptor does not match the exact live capture authority")]
    LiveCaptureAuthorityMismatch,
    #[error("claimed guardian checkpoint boundary identity does not recompute")]
    ClaimedBoundaryIdentityMismatch,
    #[error("claimed guardian checkpoint artifact identity does not recompute")]
    ClaimedCheckpointIdentityMismatch,
    #[error("Genesis checkpoint has no guardian output-record authority")]
    GenesisHasNoRecordAuthority,
    #[error("record-backed checkpoint has no Genesis Spawn authority")]
    RecordHasNoGenesisAuthority,
    #[error("Genesis checkpoint does not match the expected Spawn effect")]
    GenesisEffectIdentityMismatch,
    #[error("Genesis checkpoint does not match its retained Spawn and terminal authority")]
    GenesisCheckpointAuthorityMismatch,
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
    use crate::guardian_protocol::GuardianCheckpointDescriptorV1;
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

    fn record_stage_binding(
        descriptor: GuardianCheckpointArtifactDescriptorV1,
        generation: u64,
    ) -> GuardianCheckpointStageBindingV1 {
        let pane_id = descriptor
            .origin()
            .durable_pane_id()
            .expect("record descriptor pane");
        let scope = GuardianCheckpointStageScopeV1::pane(pane_id, generation)
            .expect("construct pane staging scope");
        GuardianCheckpointStageBindingV1::from_protocol_capture(
            scope,
            descriptor,
            generation,
        )
        .expect("bind exact protocol capture generation")
    }

    fn record_manifest_capabilities(
        binding: &GuardianCheckpointStageBindingV1,
        capture: LiveParserCheckpointAck,
        upload_id: Uuid,
        publication_id: Uuid,
        candidate_record_digest: [u8; 32],
        ordered_chunk_set_digest: [u8; 32],
    ) -> GuardianCheckpointManifestSealCapabilitiesV1 {
        let seal_request = record_seal_request(binding, &capture, upload_id);
        let assembly = GuardianCheckpointValidatedStageAssemblyV1::issue_for_test(
            seal_request,
            publication_id,
            candidate_record_digest,
            ordered_chunk_set_digest,
        )
        .expect("issue test-only validated stage assembly");
        GuardianCheckpointValidatedManifestAuthorityV1::from_live_capture(binding, capture)
            .expect("consume exact record-backed seal authority")
            .bind_seal_operation(assembly)
            .expect("bind exact canonical manifest operation")
    }

    fn record_seal_request(
        binding: &GuardianCheckpointStageBindingV1,
        capture: &LiveParserCheckpointAck,
        upload_id: Uuid,
    ) -> GuardianCheckpointStageRequestV1 {
        let (pane_id, generation) = binding
            .scope()
            .pane_identity()
            .expect("record manifest fixture uses pane scope");
        let protocol_descriptor =
            GuardianCheckpointDescriptorV1::from_live_capture(&capture, generation)
                .expect("construct authoritative protocol descriptor");
        let seal_request = GuardianCheckpointStageRequestV1::seal(
            GuardianCheckpointScopeV1::Pane {
                pane_id,
                generation,
            },
            upload_id,
            protocol_descriptor,
            4_096,
        )
        .expect("construct canonical protocol Seal request")
    }

    fn default_record_manifest_capabilities(
        binding: &GuardianCheckpointStageBindingV1,
        capture: LiveParserCheckpointAck,
        upload_id: Uuid,
        publication_id: Uuid,
    ) -> GuardianCheckpointManifestSealCapabilitiesV1 {
        record_manifest_capabilities(
            binding,
            capture,
            upload_id,
            publication_id,
            [0xa1; 32],
            [0xb2; 32],
        )
    }

    fn stage_plaintext(plaintext: &[u8]) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(plaintext.to_vec())
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
    fn claimed_descriptor_and_receipt_cannot_mint_manifest_authority() {
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

        let payload_splice = terminal_checkpoint_with(24, 80, "guardian-checkpoint-tesz");
        assert_eq!(
            payload_splice.canonical_payload().len(),
            capture.terminal_checkpoint().canonical_payload().len(),
            "claimed-payload splice must isolate content identity"
        );
        let (terminal_payload_bytes, terminal_payload_digest) =
            terminal_payload_identity(payload_splice.canonical_payload())
                .expect("identify canonical claimed payload splice");
        let claimed_splice = GuardianCheckpointArtifactDescriptorV1 {
            origin: descriptor.origin(),
            parser_stream_bytes: payload_splice.parser_stream_bytes(),
            replay_identity_digest: current_replay_identity_digest(),
            rows: u32::try_from(payload_splice.rows()).expect("fixture rows fit u32"),
            cols: u32::try_from(payload_splice.cols()).expect("fixture cols fit u32"),
            terminal_payload_bytes,
            terminal_payload_digest,
        };
        let claimed_splice = GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
            claimed_splice
                .recompute_boundary_identity_digest()
                .expect("recompute claimed splice boundary"),
            claimed_splice
                .recompute_checkpoint_identity_digest()
                .expect("recompute claimed splice artifact"),
            claimed_splice.origin(),
            claimed_splice.parser_stream_bytes(),
            claimed_splice.replay_identity_digest(),
            claimed_splice.rows(),
            claimed_splice.cols(),
            claimed_splice.terminal_payload_bytes(),
            claimed_splice.terminal_payload_digest(),
        )
        .expect("admit self-consistent claimed descriptor identity");
        claimed_splice
            .validate_record_authority(segment, first)
            .expect("claimed identity can match a real receipt without parser causality");
        claimed_splice
            .validate_canonical_payload(
                payload_splice.canonical_payload(),
                TerminalCheckpointLimits::default(),
            )
            .expect("claimed identity can match canonical caller bytes");
        let claimed_binding = record_stage_binding(claimed_splice, 11);
        assert!(matches!(
            GuardianCheckpointValidatedManifestAuthorityV1::from_live_capture(
                &claimed_binding,
                capture,
            ),
            Err(GuardianCheckpointBoundaryError::LiveCaptureAuthorityMismatch)
        ));

        let exact_capture = live_capture(
            [2; 16],
            pane,
            segment,
            first,
            terminal_checkpoint(),
        );
        let exact_descriptor =
            GuardianCheckpointArtifactDescriptorV1::from_live_capture(&exact_capture)
                .expect("construct second exact descriptor");
        let exact_binding = record_stage_binding(exact_descriptor, 11);
        GuardianCheckpointValidatedManifestAuthorityV1::from_live_capture(
            &exact_binding,
            exact_capture,
        )
        .expect("only the consumed exact live capture mints authority");
    }

    fn checkpoint_stage_cipher(seed: u8) -> GuardianCheckpointCipher {
        let output_cipher = GuardianOutputCipher::try_from_key_slice(
            &[seed; GuardianOutputCipher::KEY_BYTES],
        )
        .expect("construct checkpoint staging key fixture");
        GuardianCheckpointCipher::from_output_cipher(&output_cipher)
    }

    fn assert_authenticated_header_mutation_fails(
        cipher: &GuardianCheckpointCipher,
        original_header: [u8; GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES],
        original_ciphertext: &[u8],
        mutate: impl FnOnce(
            &mut [u8; GUARDIAN_CHECKPOINT_STAGE_RECORD_HEADER_BYTES],
            &mut Vec<u8>,
        ),
    ) {
        let mut header = original_header;
        let mut ciphertext = original_ciphertext.to_vec();
        mutate(&mut header, &mut ciphertext);
        let record = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
            &header,
            ciphertext,
            GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
        )
        .expect("AAD mutation fixture must remain structurally valid");
        let expected_context = record.context();
        assert!(matches!(
            cipher.open(
                &expected_context,
                &record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::AuthenticationFailed)
        ));
    }

    fn assert_authenticated_inner_mutation_fails(
        cipher: &GuardianCheckpointCipher,
        context: GuardianCheckpointStageRecordContextV1,
        plaintext: &[u8],
        expected_error: GuardianCheckpointCipherError,
        mutate: impl FnOnce(&mut [u8]),
    ) {
        let (_, plaintext_digest) =
            checkpoint_stage_plaintext_identity(plaintext).expect("identify inner fixture");
        let mut inner_plaintext = checkpoint_stage_inner_plaintext(plaintext, &plaintext_digest)
            .expect("construct canonical encrypted inner fixture");
        mutate(inner_plaintext.as_mut_slice());
        let aad = checkpoint_stage_record_aad(cipher.key_id(), &context);
        let (nonce, ciphertext) = cipher
            .output_cipher
            .seal_guardian_metadata(inner_plaintext.as_slice(), &aad)
            .expect("authenticate inner-envelope mutation fixture");
        drop(inner_plaintext);
        let record = GuardianEncryptedCheckpointStageRecordV1 {
            version: GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION,
            key_id: cipher.key_id(),
            nonce,
            context,
            ciphertext,
        };
        match cipher.open(
            &context,
            &record,
            GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
        ) {
            Err(observed) => assert_eq!(observed, expected_error),
            Ok(unexpected_plaintext) => {
                drop(unexpected_plaintext);
                panic!("authenticated inner-envelope mutation was accepted");
            }
        }
    }

    #[test]
    fn checkpoint_cipher_round_trips_every_typed_record_and_fixed_header() {
        let plaintext = b"bounded checkpoint staging plaintext";
        let (descriptor, _, _, capture) = record_descriptor();
        let binding = record_stage_binding(descriptor, 7);
        let upload_id = Uuid::new_v4();
        let publication_id = Uuid::new_v4();
        let intents = [
            GuardianCheckpointStageSealIntentV1::candidate_metadata(
                &binding,
                upload_id,
                publication_id,
                stage_plaintext(plaintext),
            )
            .expect("construct candidate metadata intent"),
            GuardianCheckpointStageSealIntentV1::chunk(
                &binding,
                upload_id,
                publication_id,
                0,
                0,
                stage_plaintext(plaintext),
            )
            .expect("construct chunk intent"),
        ];
        let cipher = checkpoint_stage_cipher(0x31);

        for intent in intents {
            let context = intent.context();
            let record = cipher
                .seal(intent)
                .expect("seal typed checkpoint staging record");
            assert_eq!(record.version(), GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION);
            assert_eq!(record.key_id(), cipher.key_id());
            assert_eq!(record.context(), context);
            assert_eq!(
                record.plaintext_bytes(),
                u32::try_from(plaintext.len()).expect("fixture plaintext length fits u32")
            );
            let header = record.fixed_header();
            assert_eq!(
                GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
                    &header,
                    GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
                )
                .expect("derive bounded persisted body length"),
                record.ciphertext_bytes()
            );
            let reconstructed = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &header,
                record.ciphertext().to_vec(),
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            )
            .expect("reconstruct bounded fixed-layout record");
            assert_eq!(reconstructed.context(), context);
            let opened: Zeroizing<Vec<u8>> = cipher
                .open(
                    &context,
                    &reconstructed,
                    GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
                )
                .expect("open exact typed checkpoint staging record");
            assert!(checkpoint_stage_bytes_match(opened.as_slice(), plaintext));
            drop(opened);
        }

        let capabilities = default_record_manifest_capabilities(
            &binding,
            capture,
            upload_id,
            publication_id,
        );
        let (primary, retry) = capabilities.into_primary_and_retry();
        let manifest_context = primary.context();
        let manifest_record = cipher
            .seal_manifest(primary)
            .expect("seal typed canonical final manifest");
        assert_eq!(
            manifest_record.plaintext_bytes(),
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES
        );
        let reconstructed = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
            &manifest_record.fixed_header(),
            manifest_record.ciphertext().to_vec(),
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
        )
        .expect("reconstruct bounded final manifest record");
        assert_eq!(reconstructed.context(), manifest_context);
        assert!(matches!(
            cipher.open(
                &manifest_context,
                &reconstructed,
                GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidKindAuthority)
        ));
        cipher
            .retry_open_manifest(&retry, &reconstructed)
            .expect("adopt only the exact canonical final manifest");
    }

    #[test]
    fn seal_intent_is_single_use_and_records_retain_only_persisted_claims() {
        assert!(std::mem::needs_drop::<GuardianCheckpointStageSealIntentV1>());
        assert!(std::mem::needs_drop::<
            GuardianCheckpointValidatedManifestOperationV1
        >());
        assert!(std::mem::needs_drop::<
            GuardianCheckpointManifestRetryCapabilityV1
        >());
        assert!(std::mem::needs_drop::<
            GuardianCheckpointManifestSealCapabilitiesV1
        >());
        assert!(std::mem::needs_drop::<
            GuardianCheckpointValidatedStageAssemblyV1
        >());
        assert!(!std::mem::needs_drop::<
            GuardianCheckpointStageRecordContextV1
        >());

        let source = include_str!("guardian_checkpoint.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("isolate production source");
        let intent_start = production
            .find("pub struct GuardianCheckpointStageSealIntentV1 {")
            .expect("find seal-intent declaration");
        let intent_impl_start = production[intent_start..]
            .find("impl GuardianCheckpointStageSealIntentV1 {")
            .map(|offset| intent_start + offset)
            .expect("find seal-intent implementation");
        let intent_debug_start = production[intent_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointStageSealIntentV1")
            .map(|offset| intent_impl_start + offset)
            .expect("find seal-intent Debug implementation");
        let intent_declaration = &production[intent_start..intent_impl_start];
        let intent_implementation = &production[intent_impl_start..intent_debug_start];
        assert!(intent_declaration.contains("Zeroizing<[u8; 32]>"));
        assert!(intent_declaration.contains("Zeroizing<Vec<u8>>"));
        assert!(!intent_declaration.contains("#[derive(Clone"));
        assert!(intent_implementation.contains("pub fn candidate_metadata("));
        assert!(intent_implementation.contains("pub fn chunk("));
        assert!(!intent_implementation.contains("seal_manifest"));
        assert!(!intent_implementation.contains("pub fn plaintext("));
        assert!(!intent_implementation.contains("plaintext_digest(&self"));

        let assembly_start = production
            .find("pub struct GuardianCheckpointValidatedStageAssemblyV1 {")
            .expect("find validated stage-assembly declaration");
        let assembly_impl_start = production[assembly_start..]
            .find("impl GuardianCheckpointValidatedStageAssemblyV1 {")
            .map(|offset| assembly_start + offset)
            .expect("find validated stage-assembly implementation");
        let assembly_debug_start = production[assembly_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointValidatedStageAssemblyV1")
            .map(|offset| assembly_impl_start + offset)
            .expect("find validated stage-assembly Debug implementation");
        let assembly_declaration = &production[assembly_start..assembly_impl_start];
        let assembly_implementation = &production[assembly_impl_start..assembly_debug_start];
        assert!(!assembly_declaration.contains("#[derive(Clone"));
        assert!(assembly_declaration.contains("candidate_record_digest: Zeroizing<[u8; 32]>"));
        assert!(assembly_declaration.contains("ordered_chunk_set_digest: Zeroizing<[u8; 32]>"));
        assert!(assembly_implementation.contains("#[cfg(test)]"));
        assert!(assembly_implementation.contains("fn issue_for_test("));
        assert!(!assembly_implementation.contains("pub fn "));

        let authority_start = production
            .find("pub struct GuardianCheckpointValidatedManifestAuthorityV1 {")
            .expect("find manifest authority declaration");
        let authority_impl_start = production[authority_start..]
            .find("impl GuardianCheckpointValidatedManifestAuthorityV1 {")
            .map(|offset| authority_start + offset)
            .expect("find manifest authority implementation");
        let authority_debug_start = production[authority_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointValidatedManifestAuthorityV1")
            .map(|offset| authority_impl_start + offset)
            .expect("find manifest authority Debug implementation");
        let authority_declaration = &production[authority_start..authority_impl_start];
        let authority_implementation =
            &production[authority_impl_start..authority_debug_start];
        assert!(!authority_declaration.contains("#[derive(Clone"));
        assert!(authority_implementation.contains("capture: LiveParserCheckpointAck,"));
        assert!(!authority_implementation.contains("capture: &LiveParserCheckpointAck,"));
        assert!(authority_implementation.contains("permit: GuardianCheckpointGenesisSpawnPermitV1,"));
        assert!(!authority_implementation.contains("from_claimed"));
        assert!(authority_implementation.contains("pub fn bind_seal_operation("));
        assert!(authority_implementation.contains("assembly: GuardianCheckpointValidatedStageAssemblyV1,"));
        assert!(!authority_implementation.contains("candidate_record_digest: [u8; 32]"));
        assert!(!authority_implementation.contains("ordered_chunk_set_digest: [u8; 32]"));

        let operation_start = production
            .find("pub struct GuardianCheckpointValidatedManifestOperationV1 {")
            .expect("find typed manifest operation declaration");
        let operation_impl_start = production[operation_start..]
            .find("impl GuardianCheckpointValidatedManifestOperationV1 {")
            .map(|offset| operation_start + offset)
            .expect("find typed manifest operation implementation");
        let operation_declaration = &production[operation_start..operation_impl_start];
        assert!(!operation_declaration.contains("#[derive(Clone"));
        assert!(operation_declaration.contains("canonical_manifest: Zeroizing<Vec<u8>>"));
        assert!(operation_declaration.contains("expected_manifest_digest: Zeroizing<[u8; 32]>"));
        assert!(!operation_declaration.contains("pub canonical_manifest"));

        let retry_start = production
            .find("pub struct GuardianCheckpointManifestRetryCapabilityV1 {")
            .expect("find retry capability declaration");
        let retry_impl_start = production[retry_start..]
            .find("impl GuardianCheckpointManifestRetryCapabilityV1 {")
            .map(|offset| retry_start + offset)
            .expect("find retry capability implementation");
        let retry_declaration = &production[retry_start..retry_impl_start];
        let retry_debug_start = production[retry_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointManifestRetryCapabilityV1")
            .map(|offset| retry_impl_start + offset)
            .expect("find retry capability Debug implementation");
        let retry_implementation = &production[retry_impl_start..retry_debug_start];
        assert!(!retry_declaration.contains("#[derive(Clone"));
        assert!(retry_declaration.contains("operation: GuardianCheckpointValidatedManifestOperationV1"));
        assert!(!retry_implementation.contains("into_operation"));

        let cipher_impl_start = production
            .find("impl GuardianCheckpointCipher {")
            .expect("find checkpoint cipher implementation");
        let cipher_debug_start = production[cipher_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointCipher")
            .map(|offset| cipher_impl_start + offset)
            .expect("find checkpoint cipher Debug implementation");
        let cipher_implementation = &production[cipher_impl_start..cipher_debug_start];
        assert!(cipher_implementation.contains("intent: GuardianCheckpointStageSealIntentV1,"));
        assert!(cipher_implementation.contains("operation: GuardianCheckpointValidatedManifestOperationV1,"));
        assert!(cipher_implementation.contains("retry: &GuardianCheckpointManifestRetryCapabilityV1,"));
        let manifest_method_start = cipher_implementation
            .find("    pub fn seal_manifest(")
            .expect("find typed manifest cipher method");
        let manifest_method_end = cipher_implementation[manifest_method_start..]
            .find("    fn seal_exact_payload(")
            .map(|offset| manifest_method_start + offset)
            .expect("bound typed manifest cipher method");
        let manifest_method = &cipher_implementation[manifest_method_start..manifest_method_end];
        assert!(!manifest_method.contains("Zeroizing<Vec<u8>>"));
        assert!(!manifest_method.contains("plaintext:"));

        let permit_impl_start = production
            .find("impl GuardianCheckpointGenesisSpawnPermitV1 {")
            .expect("find Genesis permit implementation");
        let permit_debug_start = production[permit_impl_start..]
            .find("impl std::fmt::Debug for GuardianCheckpointGenesisSpawnPermitV1")
            .map(|offset| permit_impl_start + offset)
            .expect("find Genesis permit Debug implementation");
        let permit_implementation = &production[permit_impl_start..permit_debug_start];
        assert!(permit_implementation.contains("#[cfg(test)]"));
        assert_eq!(permit_implementation.matches("fn ").count(), 1);
        assert!(!permit_implementation.contains("pub fn "));

        let context_start = production
            .find("pub struct GuardianCheckpointStageRecordContextV1 {")
            .expect("find persisted context declaration");
        let context_impl_start = production[context_start..]
            .find("impl GuardianCheckpointStageRecordContextV1 {")
            .map(|offset| context_start + offset)
            .expect("find persisted context implementation");
        let context_declaration = &production[context_start..context_impl_start];
        assert!(!context_declaration.contains("plaintext_digest"));
        assert!(!context_declaration.contains("authority"));

        let record_start = production
            .find("pub struct GuardianEncryptedCheckpointStageRecordV1 {")
            .expect("find encrypted-record declaration");
        let record_impl_start = production[record_start..]
            .find("impl GuardianEncryptedCheckpointStageRecordV1 {")
            .map(|offset| record_start + offset)
            .expect("find encrypted-record implementation");
        let record_declaration = &production[record_start..record_impl_start];
        assert!(!record_declaration.contains("plaintext_digest"));
        assert!(!record_declaration.contains("authority"));

        let plaintext = b"single-use zeroizing seal intent";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 7);
        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(plaintext),
        )
        .expect("construct consuming candidate intent");
        let persisted_claim = intent.context();
        let cipher = checkpoint_stage_cipher(0x7a);
        let record = cipher.seal(intent).expect("consume candidate intent");
        assert_eq!(record.context(), persisted_claim);
        let reconstructed = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
            &record.fixed_header(),
            record.ciphertext().to_vec(),
            GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
        )
        .expect("reconstruct persisted wire claim");
        assert_eq!(reconstructed.context(), persisted_claim);
    }

    #[test]
    fn repaired_checkpoint_format_rejects_source_exposed_v1_and_v2_records() {
        let plaintext = b"v3 checkpoint format fence";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 7);
        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(plaintext),
        )
        .expect("construct v3 format intent");
        let cipher = checkpoint_stage_cipher(0x7b);
        let record = cipher.seal(intent).expect("seal v3 format record");

        for (legacy_magic, legacy_version) in [(*b"FTGCPA01", 1_u32), (*b"FTGCPA02", 2_u32)] {
            let mut legacy_header = record.fixed_header();
            legacy_header[..8].copy_from_slice(&legacy_magic);
            legacy_header[8..12].copy_from_slice(&legacy_version.to_le_bytes());
            assert!(matches!(
                GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                    &legacy_header,
                    record.ciphertext().to_vec(),
                    GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
                ),
                Err(GuardianCheckpointCipherError::InvalidFixedHeader)
            ));

            let mut version_only_splice = record.fixed_header();
            version_only_splice[8..12].copy_from_slice(&legacy_version.to_le_bytes());
            assert!(matches!(
                GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                    &version_only_splice,
                    record.ciphertext().to_vec(),
                    GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
                ),
                Err(GuardianCheckpointCipherError::UnsupportedVersion { observed })
                    if observed == legacy_version
            ));
        }
    }

    #[test]
    fn checkpoint_stage_scope_is_exact_for_pane_and_genesis_descriptors() {
        let plaintext = b"scope-bound metadata";
        let (record_descriptor, _, _, _) = record_descriptor();
        let wrong_pane_scope = GuardianCheckpointStageScopeV1::pane(Uuid::new_v4(), 7)
            .expect("construct wrong pane scope");
        assert!(matches!(
            GuardianCheckpointStageBindingV1::from_protocol_capture(
                wrong_pane_scope,
                record_descriptor,
                7,
            ),
            Err(GuardianCheckpointCipherError::DescriptorScopeMismatch)
        ));
        let pane_id = record_descriptor
            .origin()
            .durable_pane_id()
            .expect("record descriptor pane");
        let pane_scope = GuardianCheckpointStageScopeV1::pane(pane_id, 7)
            .expect("construct exact pane scope");
        assert!(matches!(
            GuardianCheckpointStageBindingV1::from_protocol_capture(
                pane_scope,
                record_descriptor,
                8,
            ),
            Err(GuardianCheckpointCipherError::CaptureGenerationMismatch)
        ));
        let pane_binding = GuardianCheckpointStageBindingV1::from_protocol_capture(
            pane_scope,
            record_descriptor,
            7,
        )
        .expect("accept exact pane capture generation");
        assert_eq!(
            pane_binding
                .boundary_identity_digest()
                .expect("binding boundary identity"),
            record_descriptor
                .recompute_boundary_identity_digest()
                .expect("stable descriptor boundary")
        );
        let later_scope = GuardianCheckpointStageScopeV1::pane(pane_id, 8)
            .expect("construct later pane scope");
        let later_binding = GuardianCheckpointStageBindingV1::from_protocol_capture(
            later_scope,
            record_descriptor,
            8,
        )
        .expect("bind matching later capture generation");
        assert_ne!(pane_binding.scope(), later_binding.scope());
        assert_eq!(
            pane_binding
                .boundary_identity_digest()
                .expect("first generation boundary identity"),
            later_binding
                .boundary_identity_digest()
                .expect("later generation boundary identity"),
            "capture generation must fence staging without entering stable boundary identity"
        );
        assert_eq!(
            pane_binding
                .checkpoint_identity_digest()
                .expect("first generation artifact identity"),
            later_binding
                .checkpoint_identity_digest()
                .expect("later generation artifact identity"),
            "capture generation must fence staging without entering stable artifact identity"
        );

        let spawn_effect_id = Uuid::new_v4();
        let terminal = terminal_checkpoint();
        let genesis_descriptor =
            GuardianCheckpointArtifactDescriptorV1::from_genesis_checkpoint(
                spawn_effect_id,
                &terminal,
            )
            .expect("construct Genesis descriptor");
        let genesis_scope = GuardianCheckpointStageScopeV1::genesis(spawn_effect_id)
            .expect("construct Genesis scope");
        assert!(matches!(
            GuardianCheckpointStageBindingV1::from_protocol_capture(
                genesis_scope,
                genesis_descriptor,
                GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION + 1,
            ),
            Err(GuardianCheckpointCipherError::GenesisCaptureGenerationMismatch)
        ));
        let genesis_binding = GuardianCheckpointStageBindingV1::from_protocol_capture(
            genesis_scope,
            genesis_descriptor,
            GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION,
        )
        .expect("bind reserved Genesis capture generation");
        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &genesis_binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(plaintext),
        )
        .expect("construct Genesis candidate intent");
        let context = intent.context();
        let cipher = checkpoint_stage_cipher(0x32);
        let record = cipher
            .seal(intent)
            .expect("seal Genesis candidate metadata");
        let opened = cipher
            .open(&context, &record, context.plaintext_bytes())
            .expect("open Genesis candidate metadata");
        assert!(checkpoint_stage_bytes_match(opened.as_slice(), plaintext));
        drop(opened);

        let wrong_genesis_scope = GuardianCheckpointStageScopeV1::genesis(Uuid::new_v4())
            .expect("construct wrong Genesis scope");
        assert!(matches!(
            GuardianCheckpointStageBindingV1::from_protocol_capture(
                wrong_genesis_scope,
                genesis_descriptor,
                GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION,
            ),
            Err(GuardianCheckpointCipherError::DescriptorScopeMismatch)
        ));
    }

    #[test]
    fn seal_manifest_requires_consumed_origin_authority_and_exact_operation() {
        let candidate_plaintext = b"authority-gated checkpoint record";
        let (descriptor, segment, output, capture) = record_descriptor();
        let claimed_descriptor = GuardianCheckpointArtifactDescriptorV1::from_claimed_parts(
            descriptor
                .recompute_boundary_identity_digest()
                .expect("recompute claimed boundary"),
            descriptor
                .recompute_checkpoint_identity_digest()
                .expect("recompute claimed artifact"),
            descriptor.origin(),
            descriptor.parser_stream_bytes(),
            descriptor.replay_identity_digest(),
            descriptor.rows(),
            descriptor.cols(),
            descriptor.terminal_payload_bytes(),
            descriptor.terminal_payload_digest(),
        )
        .expect("validate claimed descriptor fields");
        let binding = record_stage_binding(claimed_descriptor, 19);
        let upload_id = Uuid::new_v4();
        let publication_id = Uuid::new_v4();
        let cipher = checkpoint_stage_cipher(0x73);

        let candidate = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            upload_id,
            publication_id,
            stage_plaintext(candidate_plaintext),
        )
        .expect("claimed descriptor authorizes candidate staging");
        cipher
            .seal(candidate)
            .expect("seal candidate under stage binding");
        let chunk = GuardianCheckpointStageSealIntentV1::chunk(
            &binding,
            upload_id,
            publication_id,
            0,
            0,
            stage_plaintext(candidate_plaintext),
        )
        .expect("claimed descriptor authorizes chunk staging");
        cipher
            .seal(chunk)
            .expect("seal chunk under stage binding");

        let mut manifest_without_authority =
            GuardianCheckpointStageSealIntentV1::candidate_metadata(
                &binding,
                upload_id,
                publication_id,
                stage_plaintext(candidate_plaintext),
            )
            .expect("construct authority-kind mutation intent");
        manifest_without_authority.context.kind =
            GuardianCheckpointStageRecordKindV1::SealManifest;
        assert!(matches!(
            cipher.seal(manifest_without_authority),
            Err(GuardianCheckpointCipherError::InvalidKindAuthority)
        ));

        let capabilities = default_record_manifest_capabilities(
            &binding,
            capture,
            upload_id,
            publication_id,
        );
        let (primary, retry) = capabilities.into_primary_and_retry();
        assert_eq!(primary.context(), retry.context());
        assert_eq!(
            primary.context().kind(),
            GuardianCheckpointStageRecordKindV1::SealManifest
        );
        assert_eq!(
            primary.context().plaintext_bytes(),
            GUARDIAN_CHECKPOINT_SEAL_MANIFEST_BYTES
        );
        let record = cipher
            .seal_manifest(primary)
            .expect("seal internally constructed canonical manifest");
        cipher
            .retry_open_manifest(&retry, &record)
            .expect("adopt only the separately bound exact retry");

        let (other_descriptor, _, _, other_capture) = record_descriptor();
        let other_binding = record_stage_binding(other_descriptor, 19);
        let other_authority = GuardianCheckpointValidatedManifestAuthorityV1::from_live_capture(
            &other_binding,
            other_capture,
        )
            .expect("derive other exact live authority");
        let target_capture = live_capture(
            [3; 16],
            descriptor
                .origin()
                .durable_pane_id()
                .expect("target record pane"),
            segment,
            output,
            terminal_checkpoint(),
        );
        let target_request = record_seal_request(&binding, &target_capture, upload_id);
        let target_assembly = GuardianCheckpointValidatedStageAssemblyV1::issue_for_test(
            target_request,
            publication_id,
            [0xa1; 32],
            [0xb2; 32],
        )
        .expect("issue mismatched test stage assembly");
        assert!(matches!(
            other_authority.bind_seal_operation(target_assembly),
            Err(GuardianCheckpointCipherError::ManifestAuthorityMismatch)
        ));

        let pane_id = descriptor
            .origin()
            .durable_pane_id()
            .expect("mutation fixture pane");
        let fresh_operation = || {
            let fresh_capture = live_capture(
                [4; 16],
                pane_id,
                segment,
                output,
                terminal_checkpoint(),
            );
            let capabilities = default_record_manifest_capabilities(
                &binding,
                fresh_capture,
                upload_id,
                publication_id,
            );
            capabilities.into_primary_and_retry().0
        };

        let request_bytes = usize::try_from(GUARDIAN_CHECKPOINT_SEAL_REQUEST_BYTES)
            .expect("request bytes fit usize");
        let mut kind_substitution = fresh_operation();
        kind_substitution.canonical_manifest[6] = 1;
        assert!(kind_substitution.validate().is_err());

        let mut scope_substitution = fresh_operation();
        scope_substitution.canonical_manifest[16] ^= 1;
        assert!(scope_substitution.validate().is_err());

        let mut upload_substitution = fresh_operation();
        upload_substitution.canonical_manifest[40] ^= 1;
        assert!(upload_substitution.validate().is_err());

        let mut descriptor_substitution = fresh_operation();
        descriptor_substitution.canonical_manifest[56] ^= 1;
        assert!(descriptor_substitution.validate().is_err());

        let mut candidate_digest_substitution = fresh_operation();
        candidate_digest_substitution.canonical_manifest[request_bytes] ^= 1;
        assert!(matches!(
            candidate_digest_substitution.validate(),
            Err(GuardianCheckpointCipherError::SealManifestIdentityMismatch)
        ));

        let mut chunk_set_digest_substitution = fresh_operation();
        chunk_set_digest_substitution.canonical_manifest[request_bytes + 32] ^= 1;
        assert!(matches!(
            chunk_set_digest_substitution.validate(),
            Err(GuardianCheckpointCipherError::SealManifestIdentityMismatch)
        ));

        let mut zero_candidate_digest = fresh_operation();
        zero_candidate_digest.canonical_manifest[request_bytes..request_bytes + 32].fill(0);
        assert!(matches!(
            zero_candidate_digest.validate(),
            Err(GuardianCheckpointCipherError::InvalidManifestComponentDigest)
        ));

        let mut length_substitution = fresh_operation();
        length_substitution.canonical_manifest.pop();
        assert!(matches!(
            length_substitution.validate(),
            Err(GuardianCheckpointCipherError::InvalidSealManifestLength)
        ));

        let mut context_kind_substitution = fresh_operation();
        context_kind_substitution.context.kind =
            GuardianCheckpointStageRecordKindV1::CandidateMetadata;
        assert!(context_kind_substitution.validate().is_err());

        let mut context_scope_substitution = fresh_operation();
        context_scope_substitution.context.scope =
            GuardianCheckpointStageScopeV1::pane(pane_id, 20)
                .expect("construct substituted scope");
        assert!(context_scope_substitution.validate().is_err());

        let mut context_upload_substitution = fresh_operation();
        context_upload_substitution.context.upload_id = Uuid::new_v4();
        assert!(context_upload_substitution.validate().is_err());

        let mut context_publication_substitution = fresh_operation();
        context_publication_substitution.context.publication_id = Uuid::new_v4();
        assert!(matches!(
            context_publication_substitution.validate(),
            Err(GuardianCheckpointCipherError::SealOperationIdentityMismatch)
        ));

        let mut context_boundary_substitution = fresh_operation();
        context_boundary_substitution.context.boundary_identity_digest[0] ^= 1;
        assert!(context_boundary_substitution.validate().is_err());

        let mut context_checkpoint_substitution = fresh_operation();
        context_checkpoint_substitution.context.checkpoint_identity_digest[0] ^= 1;
        assert!(context_checkpoint_substitution.validate().is_err());

        let mut plaintext_digest_substitution = fresh_operation();
        plaintext_digest_substitution.expected_plaintext_digest[0] ^= 1;
        assert!(matches!(
            plaintext_digest_substitution.validate(),
            Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch)
        ));

        let mut manifest_digest_substitution = fresh_operation();
        manifest_digest_substitution.expected_manifest_digest[0] ^= 1;
        assert!(matches!(
            manifest_digest_substitution.validate(),
            Err(GuardianCheckpointCipherError::SealManifestIdentityMismatch)
        ));

        let mut operation_digest_substitution = fresh_operation();
        operation_digest_substitution.expected_operation_digest[0] ^= 1;
        assert!(matches!(
            operation_digest_substitution.validate(),
            Err(GuardianCheckpointCipherError::SealOperationIdentityMismatch)
        ));

        let spawn_effect_id = Uuid::new_v4();
        let genesis_terminal = terminal_checkpoint();
        let genesis_descriptor =
            GuardianCheckpointArtifactDescriptorV1::from_genesis_checkpoint(
                spawn_effect_id,
                &genesis_terminal,
            )
            .expect("construct Genesis authority descriptor");
        let genesis_scope = GuardianCheckpointStageScopeV1::genesis(spawn_effect_id)
            .expect("construct exact Genesis scope");
        let genesis_binding = GuardianCheckpointStageBindingV1::from_protocol_capture(
            genesis_scope,
            genesis_descriptor,
            GUARDIAN_CHECKPOINT_GENESIS_STAGE_GENERATION,
        )
        .expect("bind exact Genesis capture generation");
        assert!(matches!(
            GuardianCheckpointValidatedManifestAuthorityV1::from_genesis_spawn_permit(
                &genesis_binding,
                GuardianCheckpointGenesisSpawnPermitV1::issue_for_test(Uuid::new_v4()),
                &genesis_terminal,
            ),
            Err(GuardianCheckpointBoundaryError::GenesisEffectIdentityMismatch)
        ));
        assert!(matches!(
            GuardianCheckpointValidatedManifestAuthorityV1::from_genesis_spawn_permit(
                &binding,
                GuardianCheckpointGenesisSpawnPermitV1::issue_for_test(spawn_effect_id),
                &genesis_terminal,
            ),
            Err(GuardianCheckpointBoundaryError::RecordHasNoGenesisAuthority)
        ));
        let genesis_splice = terminal_checkpoint_with(24, 80, "guardian-checkpoint-tesz");
        assert!(matches!(
            GuardianCheckpointValidatedManifestAuthorityV1::from_genesis_spawn_permit(
                &genesis_binding,
                GuardianCheckpointGenesisSpawnPermitV1::issue_for_test(spawn_effect_id),
                &genesis_splice,
            ),
            Err(GuardianCheckpointBoundaryError::GenesisCheckpointAuthorityMismatch)
        ));
        let genesis_authority =
            GuardianCheckpointValidatedManifestAuthorityV1::from_genesis_spawn_permit(
                &genesis_binding,
                GuardianCheckpointGenesisSpawnPermitV1::issue_for_test(spawn_effect_id),
                &genesis_terminal,
            )
            .expect("mint exact Genesis terminal and retained Spawn authority");
        let genesis_upload_id = Uuid::new_v4();
        let genesis_publication_id = Uuid::new_v4();
        let genesis_protocol_descriptor =
            GuardianCheckpointDescriptorV1::for_genesis_artifact(
                spawn_effect_id,
                &genesis_terminal,
            )
            .expect("construct authoritative Genesis protocol descriptor");
        let genesis_request = GuardianCheckpointStageRequestV1::seal(
            GuardianCheckpointScopeV1::Genesis { spawn_effect_id },
            genesis_upload_id,
            genesis_protocol_descriptor,
            4_096,
        )
        .expect("construct canonical Genesis Seal request");
        let genesis_assembly = GuardianCheckpointValidatedStageAssemblyV1::issue_for_test(
            genesis_request,
            genesis_publication_id,
            [0xc3; 32],
            [0xd4; 32],
        )
        .expect("issue test-only Genesis stage assembly");
        let genesis_capabilities = genesis_authority
            .bind_seal_operation(genesis_assembly)
            .expect("bind exact Genesis manifest operation");
        let (genesis_primary, genesis_retry) =
            genesis_capabilities.into_primary_and_retry();
        let genesis_record = cipher
            .seal_manifest(genesis_primary)
            .expect("seal Genesis manifest under retained Spawn authority");
        cipher
            .retry_open_manifest(&genesis_retry, &genesis_record)
            .expect("adopt exact Genesis manifest retry");
    }

    #[test]
    fn every_checkpoint_stage_aad_field_is_mutation_sensitive() {
        const HEADER_CONTEXT_OFFSET: usize = 48;
        const CONTEXT_KIND_OFFSET: usize = HEADER_CONTEXT_OFFSET;
        const CONTEXT_SCOPE_TAG_OFFSET: usize = HEADER_CONTEXT_OFFSET + 1;
        const CONTEXT_SCOPE_ID_OFFSET: usize = HEADER_CONTEXT_OFFSET + 8;
        const CONTEXT_GENERATION_OFFSET: usize = HEADER_CONTEXT_OFFSET + 24;
        const CONTEXT_UPLOAD_OFFSET: usize = HEADER_CONTEXT_OFFSET + 32;
        const CONTEXT_BOUNDARY_OFFSET: usize = HEADER_CONTEXT_OFFSET + 48;
        const CONTEXT_CHECKPOINT_OFFSET: usize = HEADER_CONTEXT_OFFSET + 80;
        const CONTEXT_PUBLICATION_OFFSET: usize = HEADER_CONTEXT_OFFSET + 112;
        const CONTEXT_CHUNK_INDEX_OFFSET: usize = HEADER_CONTEXT_OFFSET + 128;
        const CONTEXT_CHUNK_OFFSET_OFFSET: usize = HEADER_CONTEXT_OFFSET + 136;
        const CONTEXT_PLAINTEXT_BYTES_OFFSET: usize = HEADER_CONTEXT_OFFSET + 144;
        const FORMER_CLEAR_DIGEST_OFFSET: usize = HEADER_CONTEXT_OFFSET + 152;

        let plaintext = b"phase-a AAD mutation fixture";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 7);
        let intent = GuardianCheckpointStageSealIntentV1::chunk(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            0,
            0,
            stage_plaintext(plaintext),
        )
        .expect("construct chunk intent");
        let context = intent.context();
        let cipher = checkpoint_stage_cipher(0x33);
        let record = cipher.seal(intent).expect("seal AAD fixture");
        let header = record.fixed_header();
        assert_eq!(&header[FORMER_CLEAR_DIGEST_OFFSET..], &[0; 32]);
        let aad = checkpoint_stage_record_aad(cipher.key_id(), &context);
        let expected_domain = b"frankenterm.guardian-checkpoint-phase-a-record.v2\0";
        assert_eq!(&aad[..expected_domain.len()], expected_domain);
        assert_eq!(
            &aad[expected_domain.len()..expected_domain.len() + 4],
            &GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION.to_le_bytes()
        );
        assert_eq!(
            &aad[expected_domain.len() + 4..expected_domain.len() + 12],
            &cipher.key_id()
        );
        assert_eq!(&aad[aad.len() - 32..], &[0; 32]);

        for offset in [
            CONTEXT_KIND_OFFSET,
            CONTEXT_SCOPE_ID_OFFSET,
            CONTEXT_GENERATION_OFFSET,
            CONTEXT_UPLOAD_OFFSET,
            CONTEXT_BOUNDARY_OFFSET,
            CONTEXT_CHECKPOINT_OFFSET,
            CONTEXT_PUBLICATION_OFFSET,
            CONTEXT_CHUNK_INDEX_OFFSET,
            CONTEXT_CHUNK_OFFSET_OFFSET,
        ] {
            assert_authenticated_header_mutation_fails(
                &cipher,
                header,
                record.ciphertext(),
                |mutated_header, _| mutated_header[offset] ^= 1,
            );
        }

        assert_authenticated_header_mutation_fails(
            &cipher,
            header,
            record.ciphertext(),
            |mutated_header, _| {
                mutated_header[CONTEXT_SCOPE_TAG_OFFSET] = 2;
                mutated_header[CONTEXT_GENERATION_OFFSET..CONTEXT_GENERATION_OFFSET + 8].fill(0);
            },
        );
        assert_authenticated_header_mutation_fails(
            &cipher,
            header,
            record.ciphertext(),
            |mutated_header, mutated_ciphertext| {
                let shorter = context
                    .plaintext_bytes()
                    .checked_sub(1)
                    .expect("fixture plaintext has more than one byte");
                mutated_header
                    [CONTEXT_PLAINTEXT_BYTES_OFFSET..CONTEXT_PLAINTEXT_BYTES_OFFSET + 4]
                    .copy_from_slice(&shorter.to_le_bytes());
                mutated_ciphertext.truncate(
                    usize::try_from(shorter).expect("fixture length fits usize")
                        + CHECKPOINT_STAGE_INNER_TRAILER_BYTES
                        + CHECKPOINT_STAGE_AEAD_TAG_BYTES,
                );
            },
        );

        let mut clear_digest_mutation = header;
        clear_digest_mutation[FORMER_CLEAR_DIGEST_OFFSET] = 1;
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &clear_digest_mutation,
                record.ciphertext().to_vec(),
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidFixedHeader)
        ));

        let mut wrong_version = header;
        wrong_version[8..12].copy_from_slice(
            &GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION
                .checked_add(1)
                .expect("format version has mutation room")
                .to_le_bytes(),
        );
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &wrong_version,
                record.ciphertext().to_vec(),
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn checkpoint_cipher_rejects_wrong_key_context_nonce_and_ciphertext() {
        let plaintext = b"authenticated checkpoint record";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 9);
        let upload_id = Uuid::new_v4();
        let intent = GuardianCheckpointStageSealIntentV1::chunk(
            &binding,
            upload_id,
            Uuid::new_v4(),
            0,
            0,
            stage_plaintext(plaintext),
        )
        .expect("construct authenticated chunk intent");
        let context = intent.context();
        let cipher = checkpoint_stage_cipher(0x41);
        let wrong_cipher = checkpoint_stage_cipher(0x42);
        let record = cipher
            .seal(intent)
            .expect("seal authenticated fixture");

        assert!(matches!(
            wrong_cipher.open(
                &context,
                &record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::KeyIdentityMismatch)
        ));

        let wrong_intent = GuardianCheckpointStageSealIntentV1::chunk(
            &binding,
            Uuid::new_v4(),
            context.publication_id(),
            0,
            0,
            stage_plaintext(plaintext),
        )
        .expect("construct wrong expected intent");
        let wrong_context = wrong_intent.context();
        drop(wrong_intent);
        assert!(matches!(
            cipher.open(
                &wrong_context,
                &record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::ContextMismatch)
        ));

        let header = record.fixed_header();
        assert_authenticated_header_mutation_fails(
            &cipher,
            header,
            record.ciphertext(),
            |mutated_header, _| mutated_header[24] ^= 1,
        );
        assert_authenticated_header_mutation_fails(
            &cipher,
            header,
            record.ciphertext(),
            |_, mutated_ciphertext| mutated_ciphertext[0] ^= 1,
        );

        let mut wrong_key_header = header;
        wrong_key_header[16..24].copy_from_slice(&wrong_cipher.key_id());
        let wrong_key_record = GuardianEncryptedCheckpointStageRecordV1::from_persisted(
            &wrong_key_header,
            record.ciphertext().to_vec(),
            GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
        )
        .expect("reconstruct record under claimed wrong key identity");
        assert!(matches!(
            wrong_cipher.open(
                &context,
                &wrong_key_record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::AuthenticationFailed)
        ));
    }

    #[test]
    fn checkpoint_record_reconstruction_is_strictly_bounded() {
        let plaintext = b"bounded persisted checkpoint record";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 11);
        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(plaintext),
        )
        .expect("construct candidate intent");
        let context = intent.context();
        let oversized_bytes =
            usize::try_from(GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES + 1)
                .expect("staging bound fits usize");
        let oversized_plaintext = Zeroizing::new(vec![0x5a; oversized_bytes]);
        assert!(matches!(
            GuardianCheckpointStageSealIntentV1::candidate_metadata(
                &binding,
                Uuid::new_v4(),
                Uuid::new_v4(),
                oversized_plaintext,
            ),
            Err(GuardianCheckpointCipherError::PlaintextByteLimit)
        ));
        let cipher = checkpoint_stage_cipher(0x51);
        let record = cipher
            .seal(intent)
            .expect("seal reconstruction fixture");
        let header = record.fixed_header();

        let mut truncated_ciphertext = record.ciphertext().to_vec();
        truncated_ciphertext.pop();
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &header,
                truncated_ciphertext,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::CiphertextLengthMismatch)
        ));
        let mut extended_ciphertext = record.ciphertext().to_vec();
        extended_ciphertext.push(0);
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &header,
                extended_ciphertext,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::CiphertextLengthMismatch)
        ));
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &header[..header.len() - 1],
                record.ciphertext().to_vec(),
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidFixedHeader)
        ));
        let mut extended_header = header.to_vec();
        extended_header.push(0);
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &extended_header,
                record.ciphertext().to_vec(),
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidFixedHeader)
        ));

        let too_small = context
            .plaintext_bytes()
            .checked_sub(1)
            .expect("fixture plaintext has more than one byte");
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::from_persisted(
                &header,
                record.ciphertext().to_vec(),
                too_small,
            ),
            Err(GuardianCheckpointCipherError::PlaintextByteLimit)
        ));
        assert!(matches!(
            cipher.open(&context, &record, too_small),
            Err(GuardianCheckpointCipherError::PlaintextByteLimit)
        ));
        assert!(matches!(
            cipher.open(
                &context,
                &record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES + 1,
            ),
            Err(GuardianCheckpointCipherError::InvalidCallerLimit)
        ));
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(&header, 0),
            Err(GuardianCheckpointCipherError::InvalidCallerLimit)
        ));

        let mut wrong_magic = header;
        wrong_magic[0] ^= 1;
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
                &wrong_magic,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidFixedHeader)
        ));
        let mut noncanonical_reserved = header;
        noncanonical_reserved[12] = 1;
        assert!(matches!(
            GuardianEncryptedCheckpointStageRecordV1::persisted_ciphertext_bytes(
                &noncanonical_reserved,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::InvalidFixedHeader)
        ));
    }

    #[test]
    fn checkpoint_cipher_uses_fresh_random_nonces() {
        let (descriptor, _, _, capture) = record_descriptor();
        let binding = record_stage_binding(descriptor, 13);
        let upload_id = Uuid::new_v4();
        let publication_id = Uuid::new_v4();
        let capabilities = default_record_manifest_capabilities(
            &binding,
            capture,
            upload_id,
            publication_id,
        );
        let (primary, retry) = capabilities.into_primary_and_retry();
        let context = primary.context();
        assert_eq!(retry.context(), context);
        let cipher = checkpoint_stage_cipher(0x61);
        let first = cipher
            .seal_manifest(primary)
            .expect("seal primary manifest record");
        let second = cipher
            .retry_seal_manifest(&retry)
            .expect("seal first exact retry manifest record");
        let third = cipher
            .retry_seal_manifest(&retry)
            .expect("seal second exact retry after another transient failure");
        let first_header = first.fixed_header();
        let second_header = second.fixed_header();
        let third_header = third.fixed_header();

        assert_ne!(&first_header[24..48], &second_header[24..48]);
        assert_ne!(&first_header[24..48], &third_header[24..48]);
        assert_ne!(&second_header[24..48], &third_header[24..48]);
        assert_ne!(first.ciphertext(), second.ciphertext());
        assert_ne!(first.ciphertext(), third.ciphertext());
        assert_ne!(second.ciphertext(), third.ciphertext());
        assert_eq!(first.context(), context);
        assert_eq!(second.context(), context);
        assert_eq!(third.context(), context);
        cipher
            .retry_open_manifest(&retry, &first)
            .expect("reconcile primary under exact retry capability");
        cipher
            .retry_open_manifest(&retry, &second)
            .expect("reconcile first retry under the same exact capability");
        cipher
            .retry_open_manifest(&retry, &third)
            .expect("reconcile second retry under the same exact capability");
    }

    #[test]
    fn checkpoint_record_debug_is_content_free() {
        let plaintext = b"CHECKPOINT-PLAINTEXT-MUST-NOT-APPEAR";
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 15);
        let intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(plaintext),
        )
        .expect("construct debug intent");
        let context = intent.context();
        let intent_debug = format!("{intent:?}");
        let cipher = checkpoint_stage_cipher(0x71);
        let record = cipher.seal(intent).expect("seal debug fixture");
        let header = record.fixed_header();
        let debug = format!("{intent_debug} {record:?} {context:?} {cipher:?}");

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("CHECKPOINT-PLAINTEXT-MUST-NOT-APPEAR"));
        let (_, plaintext_digest) =
            checkpoint_stage_plaintext_identity(plaintext).expect("identify debug fixture");
        assert!(!debug.contains(&hex::encode(&*plaintext_digest)));
        assert!(!debug.contains(&hex::encode(record.ciphertext())));
        assert!(!debug.contains(&hex::encode(&header[24..48])));
        assert_eq!(&header[200..232], &[0; 32]);
    }

    #[test]
    fn checkpoint_open_returns_zeroizing_and_reidentifies_decrypted_plaintext() {
        fn require_zeroizing(_: &Zeroizing<Vec<u8>>) {}

        let expected_plaintext = b"alpha-secret";
        let substituted_plaintext = b"omega-secret";
        assert_eq!(expected_plaintext.len(), substituted_plaintext.len());
        let (descriptor, _, _, _) = record_descriptor();
        let binding = record_stage_binding(descriptor, 17);
        let candidate_intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            Uuid::new_v4(),
            Uuid::new_v4(),
            stage_plaintext(expected_plaintext),
        )
        .expect("construct candidate intent");
        let candidate_context = candidate_intent.context();
        let cipher = checkpoint_stage_cipher(0x72);

        let mut substituted_intent = candidate_intent;
        for (destination, source) in substituted_intent
            .plaintext
            .iter_mut()
            .zip(substituted_plaintext.iter())
        {
            *destination = *source;
        }
        assert!(matches!(
            cipher.seal(substituted_intent),
            Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch)
        ));

        let mut length_spliced_intent =
            GuardianCheckpointStageSealIntentV1::candidate_metadata(
                &binding,
                candidate_context.upload_id(),
                candidate_context.publication_id(),
                stage_plaintext(expected_plaintext),
            )
            .expect("construct length-splice intent");
        length_spliced_intent.context.plaintext_bytes = length_spliced_intent
            .context
            .plaintext_bytes
            .checked_sub(1)
            .expect("fixture plaintext has more than one byte");
        assert!(matches!(
            cipher.seal(length_spliced_intent),
            Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch)
        ));

        let valid_intent = GuardianCheckpointStageSealIntentV1::candidate_metadata(
            &binding,
            candidate_context.upload_id(),
            candidate_context.publication_id(),
            stage_plaintext(expected_plaintext),
        )
        .expect("construct exact candidate intent");
        let valid_record = cipher
            .seal(valid_intent)
            .expect("seal exact candidate plaintext");
        let opened: Zeroizing<Vec<u8>> = cipher
            .open(
                &candidate_context,
                &valid_record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            )
            .expect("open exact candidate plaintext");
        require_zeroizing(&opened);
        assert!(checkpoint_stage_bytes_match(
            opened.as_slice(),
            expected_plaintext
        ));
        drop(opened);

        let aad = checkpoint_stage_record_aad(cipher.key_id(), &candidate_context);
        let (_, expected_digest) = checkpoint_stage_plaintext_identity(expected_plaintext)
            .expect("identify authenticated substitution fixture");
        let substituted_inner =
            checkpoint_stage_inner_plaintext(substituted_plaintext, &expected_digest)
                .expect("construct authenticated inner-envelope splice fixture");
        let (nonce, ciphertext) = cipher
            .output_cipher
            .seal_guardian_metadata(substituted_inner.as_slice(), &aad)
            .expect("construct authenticated substituted-plaintext fixture");
        drop(substituted_inner);
        let substituted_record = GuardianEncryptedCheckpointStageRecordV1 {
            version: GUARDIAN_CHECKPOINT_STAGE_RECORD_VERSION,
            key_id: cipher.key_id(),
            nonce,
            context: candidate_context,
            ciphertext,
        };
        assert!(matches!(
            cipher.open(
                &candidate_context,
                &substituted_record,
                GUARDIAN_CHECKPOINT_STAGE_MAX_PLAINTEXT_BYTES,
            ),
            Err(GuardianCheckpointCipherError::PlaintextIdentityMismatch)
        ));

        let trailer_offset = expected_plaintext.len();
        assert_authenticated_inner_mutation_fails(
            &cipher,
            candidate_context,
            expected_plaintext,
            GuardianCheckpointCipherError::PlaintextIdentityMismatch,
            |inner| inner[0] ^= 1,
        );
        assert_authenticated_inner_mutation_fails(
            &cipher,
            candidate_context,
            expected_plaintext,
            GuardianCheckpointCipherError::InvalidInnerEnvelope,
            |inner| inner[trailer_offset] ^= 1,
        );
        assert_authenticated_inner_mutation_fails(
            &cipher,
            candidate_context,
            expected_plaintext,
            GuardianCheckpointCipherError::InvalidInnerEnvelope,
            |inner| inner[trailer_offset + 8] ^= 1,
        );
        assert_authenticated_inner_mutation_fails(
            &cipher,
            candidate_context,
            expected_plaintext,
            GuardianCheckpointCipherError::InvalidInnerEnvelope,
            |inner| inner[trailer_offset + 12] ^= 1,
        );
        assert_authenticated_inner_mutation_fails(
            &cipher,
            candidate_context,
            expected_plaintext,
            GuardianCheckpointCipherError::PlaintextIdentityMismatch,
            |inner| inner[trailer_offset + 16] ^= 1,
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
