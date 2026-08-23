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
use sha2::{Digest as _, Sha256};
use std::convert::TryFrom;
use termwiz::escape::parser::{Parser, RECOVERY_CHECKPOINT_PARSER_ID};
use thiserror::Error;
use uuid::Uuid;

const PARSER_ID_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-checkpoint-parser-id.v1\0";

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
    parser_identity_digest: [u8; 32],
    rows: u32,
    cols: u32,
}

impl GuardianCheckpointBoundary {
    /// Capture a boundary from the exact output receipt already synchronized by
    /// the guardian. The parser and terminal model must include that record and
    /// no later record when this method is called.
    pub fn capture(
        parser: &Parser,
        segment: GuardianOutputSegmentIdentity,
        output: GuardianOutputAppendReceipt,
        rows: usize,
        cols: usize,
    ) -> Result<Self, GuardianCheckpointBoundaryError> {
        if !parser.is_recovery_ground() {
            return Err(GuardianCheckpointBoundaryError::ParserNotRecoveryGround);
        }
        if output.segment_id() != segment.segment_id() {
            return Err(GuardianCheckpointBoundaryError::OutputSegmentMismatch {
                expected: segment.segment_id(),
                observed: output.segment_id(),
            });
        }
        if output.sequence() < segment.first_sequence() {
            return Err(GuardianCheckpointBoundaryError::OutputBeforeSegment {
                first: segment.first_sequence(),
                observed: output.sequence(),
            });
        }
        let rows = u32::try_from(rows)
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        let cols = u32::try_from(cols)
            .map_err(|_| GuardianCheckpointBoundaryError::GeometryOutOfRange)?;
        if rows == 0 || cols == 0 {
            return Err(GuardianCheckpointBoundaryError::ZeroGeometry);
        }

        Ok(Self {
            version: GUARDIAN_CHECKPOINT_BOUNDARY_VERSION,
            durable_pane_id: segment.durable_pane_id(),
            segment_id: output.segment_id(),
            output_sequence: output.sequence(),
            output_record_digest: output.record_digest(),
            parser_identity_digest: current_parser_identity_digest(),
            rows,
            cols,
        })
    }

    /// Validate a decoded boundary before terminal-state bytes are admitted.
    pub fn validate_for_restore(&self) -> Result<(), GuardianCheckpointBoundaryError> {
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
        let expected = current_parser_identity_digest();
        if self.parser_identity_digest != expected {
            return Err(GuardianCheckpointBoundaryError::ParserIdentityMismatch);
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
    pub const fn parser_identity_digest(&self) -> [u8; 32] {
        self.parser_identity_digest
    }

    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.cols
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
            .field("parser_identity_digest", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .finish()
    }
}

/// Current fixed parser compatibility identity, domain-separated for use in a
/// guardian checkpoint header.
#[must_use]
pub fn current_parser_identity_digest() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PARSER_ID_DIGEST_DOMAIN);
    hasher.update(RECOVERY_CHECKPOINT_PARSER_ID.as_bytes());
    hasher.finalize().into()
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum GuardianCheckpointBoundaryError {
    #[error("terminal parser is not at a recovery-ground output boundary")]
    ParserNotRecoveryGround,
    #[error("checkpoint output segment mismatch: expected {expected}, observed {observed}")]
    OutputSegmentMismatch { expected: Uuid, observed: Uuid },
    #[error("checkpoint output sequence {observed} precedes segment base {first}")]
    OutputBeforeSegment { first: u64, observed: u64 },
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
    #[error("guardian checkpoint parser identity is incompatible")]
    ParserIdentityMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardian_output_journal::{
        GuardianOutputCipher, GuardianOutputJournal, GuardianOutputJournalLimits,
    };
    use std::fs::File;
    use tempfile::tempdir;

    fn synchronized_output(
        pane: Uuid,
    ) -> (
        GuardianOutputSegmentIdentity,
        GuardianOutputAppendReceipt,
    ) {
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
        let receipt = journal
            .append_and_sync(b"checkpoint boundary")
            .expect("synchronize output record");
        (identity, receipt)
    }

    #[test]
    fn capture_binds_ground_parser_to_exact_synchronized_output() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let parser = Parser::new();

        let boundary = GuardianCheckpointBoundary::capture(&parser, segment, output, 24, 80)
            .expect("capture exact checkpoint boundary");

        assert_eq!(boundary.durable_pane_id(), pane);
        assert_eq!(boundary.segment_id(), output.segment_id());
        assert_eq!(boundary.output_sequence(), output.sequence());
        assert_eq!(boundary.output_record_digest(), output.record_digest());
        assert_eq!(boundary.parser_identity_digest(), current_parser_identity_digest());
        boundary
            .validate_for_restore()
            .expect("current parser accepts its boundary");
    }

    #[test]
    fn partial_parser_state_cannot_publish_a_boundary() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let mut parser = Parser::new();
        parser.parse(b"\x1b]2;unfinished", |_| {});

        assert_eq!(
            GuardianCheckpointBoundary::capture(&parser, segment, output, 24, 80),
            Err(GuardianCheckpointBoundaryError::ParserNotRecoveryGround)
        );
    }

    #[test]
    fn receipt_from_another_segment_cannot_publish_a_boundary() {
        let pane = Uuid::new_v4();
        let (segment, _) = synchronized_output(pane);
        let (_, other_output) = synchronized_output(pane);

        assert!(matches!(
            GuardianCheckpointBoundary::capture(
                &Parser::new(),
                segment,
                other_output,
                24,
                80
            ),
            Err(GuardianCheckpointBoundaryError::OutputSegmentMismatch { .. })
        ));
    }

    #[test]
    fn debug_omits_content_derived_digests() {
        let pane = Uuid::new_v4();
        let (segment, output) = synchronized_output(pane);
        let boundary = GuardianCheckpointBoundary::capture(
            &Parser::new(),
            segment,
            output,
            24,
            80,
        )
        .expect("capture boundary");
        let debug = format!("{boundary:?}");

        assert!(!debug.contains(&hex::encode(output.record_digest())));
        assert!(!debug.contains(&hex::encode(current_parser_identity_digest())));
        assert_eq!(debug.matches("[REDACTED]").count(), 2);
    }
}
