//! Exact parser/output boundary authority for durable guardian checkpoints.
//!
//! A terminal-state payload is safe to publish only when it describes the
//! model produced through one exact synchronized output-journal receipt and
//! the escape parser can be replaced by a fresh parser at that same boundary.
//! This module owns that cross-subsystem identity. It intentionally does not
//! serialize terminal state; the terminal checkpoint codec must first produce
//! and validate its own bounded semantic payload, then bind it to this value.

use crate::guardian_output_journal::{
    GuardianOutputAppendReceipt, GuardianOutputSegmentIdentity,
};
use frankenterm_term::{
    RECOVERY_TERMINAL_REPLAY_SEMANTICS_ID, RecoveryTerminalCheckpointV2,
};
use sha2::{Digest as _, Sha256};
use std::convert::TryFrom;
use termwiz::escape::parser::RECOVERY_CHECKPOINT_PARSER_ID;
use thiserror::Error;
use uuid::Uuid;

const REPLAY_IDENTITY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-replay-identity.v1\0";
const TERMINAL_PAYLOAD_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-terminal-payload.v1\0";

/// Version of the cross-subsystem checkpoint-boundary contract.
pub const GUARDIAN_CHECKPOINT_BOUNDARY_VERSION: u32 = 1;

/// Exact synchronized raw-output position at which a terminal checkpoint was
/// captured while the parser was recovery-ground.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianCheckpointBoundary {
    version: u32,
    durable_pane_id: Uuid,
    segment_id: Uuid,
    output_sequence: u64,
    output_record_digest: [u8; 32],
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
    pub fn capture(
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
        {
            return Err(GuardianCheckpointBoundaryError::VerifiedOutputIdentityMismatch);
        }
        let (observed_payload_bytes, observed_payload_digest) =
            terminal_payload_identity(canonical_terminal_payload)?;
        if self.terminal_payload_bytes != observed_payload_bytes {
            return Err(GuardianCheckpointBoundaryError::TerminalPayloadLengthMismatch);
        }
        if self.terminal_payload_digest != observed_payload_digest {
            return Err(GuardianCheckpointBoundaryError::TerminalPayloadDigestMismatch);
        }
        Ok(())
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
            .field("replay_identity_digest", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("terminal_payload_bytes", &self.terminal_payload_bytes)
            .field("terminal_payload_digest", &"[REDACTED]")
            .finish()
    }
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
    #[error("checkpoint geometry does not fit the v1 format")]
    GeometryOutOfRange,
    #[error("checkpoint geometry must have nonzero rows and columns")]
    ZeroGeometry,
    #[error("unsupported guardian checkpoint boundary version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("guardian checkpoint has a nil pane identity")]
    NilPaneIdentity,
    #[error("guardian checkpoint has a nil output segment identity")]
    NilSegmentIdentity,
    #[error("guardian checkpoint output sequence must be nonzero")]
    ZeroOutputSequence,
    #[error("guardian checkpoint replay semantics identity is incompatible")]
    ReplayIdentityMismatch,
    #[error("guardian checkpoint terminal payload must be nonempty")]
    EmptyTerminalPayload,
    #[error("guardian checkpoint terminal payload length does not fit the v1 format")]
    TerminalPayloadLengthOutOfRange,
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

    fn terminal_checkpoint() -> RecoveryTerminalCheckpointV2 {
        Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(CheckpointTerminalConfig),
            "FrankenTerm",
            "guardian-checkpoint-test",
            Box::new(Vec::<u8>::new()),
        )
        .capture_recovery_checkpoint(TerminalCheckpointLimits::default())
        .expect("capture canonical terminal fixture")
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
