//! Durable input-effect intent and disposition journal for the guardian.
//!
//! An input intent and then `AcceptedNotDurable` are synchronized before any
//! bytes may become observable to a child PTY. The caller then refines that
//! conservative marker to `DurableFull`, to an exact `DurablePrefix`, or to
//! `KnownNotApplied` only when it can prove that zero bytes became observable.
//! A crash after the accepted marker is never interpreted as permission to
//! replay input; recovery retains the ambiguous effect and takeover stays
//! fenced. A durable prefix is terminal for the exact request: retries return
//! its original receipt and must never apply the known prefix again.
//!
//! Raw input is never persisted. Even its payload digest is encrypted because
//! hashes of small key events are enumerable. The fixed-size encrypted records
//! use the guardian journal key with an input-specific AEAD domain. This module
//! accepts only caller-owned file descriptors; secure path traversal and key
//! provisioning remain service-layer responsibilities.

use crate::guardian_output_journal::{GuardianOutputCipher, GuardianOutputJournalError};
use crate::guardian_protocol::{
    GUARDIAN_MAX_INPUT_BYTES, GuardianInputEffectIdentity, GuardianProtocolError,
    GuardianProtocolState, GuardianReply, InputEffectState,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use thiserror::Error;
use uuid::Uuid;

const FILE_MAGIC: [u8; 8] = *b"FTGINP02";
const RECORD_MAGIC: [u8; 8] = *b"FTGIR002";
const LEGACY_FILE_MAGIC_V1: [u8; 8] = *b"FTGINP01";
const FORMAT_VERSION: u32 = 2;
const FILE_HEADER_BYTES: usize = 128;
const FILE_HEADER_BYTES_U32: u32 = 128;
const FILE_HEADER_BYTES_U64: u64 = 128;
const RECORD_HEADER_BYTES: usize = 96;
const RECORD_HEADER_BYTES_U32: u32 = 96;
const RECORD_HEADER_BYTES_U64: u64 = 96;
const RECORD_PLAINTEXT_BYTES: usize = 136;
const RECORD_PLAINTEXT_BYTES_U32: u32 = 136;
const RECORD_CIPHERTEXT_BYTES: usize = 152;
const RECORD_CIPHERTEXT_BYTES_U32: u32 = 152;
const RECORD_BYTES_U64: u64 = 248;
const NONCE_BYTES: usize = 24;
const KEY_ID_BYTES: usize = 8;
const FILE_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-input-file.v2\0";
const RECORD_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-input-record.v2\0";
const RECORD_AAD_DOMAIN: &[u8] = b"frankenterm.guardian-input-aead.v2\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianInputJournalLimits {
    pub max_log_bytes: u64,
    pub max_records: u64,
    pub max_effects: usize,
    pub max_input_bytes: u32,
}

impl Default for GuardianInputJournalLimits {
    fn default() -> Self {
        let max_input_bytes = u32::try_from(GUARDIAN_MAX_INPUT_BYTES)
            .expect("guardian protocol input bound fits in u32");
        Self {
            max_log_bytes: 256 * 1024 * 1024,
            max_records: 1_000_000,
            max_effects: 65_536,
            max_input_bytes,
        }
    }
}

impl GuardianInputJournalLimits {
    fn validate(self) -> Result<Self, GuardianInputJournalError> {
        let protocol_max_input_bytes = u32::try_from(GUARDIAN_MAX_INPUT_BYTES)
            .map_err(|_| GuardianInputJournalError::InvalidLimits)?;
        if self.max_records == 0
            || self.max_effects == 0
            || self.max_input_bytes == 0
            || self.max_input_bytes > protocol_max_input_bytes
        {
            return Err(GuardianInputJournalError::InvalidLimits);
        }
        let minimum = FILE_HEADER_BYTES_U64
            .checked_add(RECORD_BYTES_U64)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        if self.max_log_bytes < minimum {
            return Err(GuardianInputJournalError::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianInputDisposition {
    Intent,
    AcceptedNotDurable,
    DurableFull,
    DurablePrefix { applied_bytes: u32 },
    KnownNotApplied,
}

impl GuardianInputDisposition {
    #[must_use]
    pub const fn applied_prefix_bytes(self) -> Option<u32> {
        match self {
            Self::DurablePrefix { applied_bytes } => Some(applied_bytes),
            Self::Intent
            | Self::AcceptedNotDurable
            | Self::DurableFull
            | Self::KnownNotApplied => None,
        }
    }

    const fn tag(self) -> GuardianInputDispositionTag {
        match self {
            Self::Intent => GuardianInputDispositionTag::Intent,
            Self::AcceptedNotDurable => GuardianInputDispositionTag::AcceptedNotDurable,
            Self::DurableFull => GuardianInputDispositionTag::DurableFull,
            Self::DurablePrefix { .. } => GuardianInputDispositionTag::DurablePrefix,
            Self::KnownNotApplied => GuardianInputDispositionTag::KnownNotApplied,
        }
    }

    const fn stored_applied_bytes(self) -> u32 {
        match self {
            Self::DurablePrefix { applied_bytes } => applied_bytes,
            Self::Intent
            | Self::AcceptedNotDurable
            | Self::DurableFull
            | Self::KnownNotApplied => 0,
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::DurableFull | Self::DurablePrefix { .. } | Self::KnownNotApplied
        )
    }

    fn from_stored(
        tag: GuardianInputDispositionTag,
        applied_bytes: u32,
        input_bytes: u32,
    ) -> Result<Self, GuardianInputJournalError> {
        let disposition = match (tag, applied_bytes) {
            (GuardianInputDispositionTag::Intent, 0) => Self::Intent,
            (GuardianInputDispositionTag::AcceptedNotDurable, 0) => Self::AcceptedNotDurable,
            (GuardianInputDispositionTag::DurableFull, 0) => Self::DurableFull,
            (GuardianInputDispositionTag::KnownNotApplied, 0) => Self::KnownNotApplied,
            (GuardianInputDispositionTag::DurablePrefix, applied_bytes) => {
                Self::DurablePrefix { applied_bytes }
            }
            _ => return Err(GuardianInputJournalError::InvalidAppliedByteCount),
        };
        disposition.validate_for_input_bytes(input_bytes)?;
        Ok(disposition)
    }

    fn validate_for_input_bytes(
        self,
        input_bytes: u32,
    ) -> Result<(), GuardianInputJournalError> {
        if input_bytes == 0 {
            return Err(GuardianInputJournalError::InvalidInputLength);
        }
        if matches!(
            self,
            Self::DurablePrefix { applied_bytes }
                if applied_bytes == 0 || applied_bytes >= input_bytes
        ) {
            return Err(GuardianInputJournalError::InvalidAppliedByteCount);
        }
        Ok(())
    }

    /// Return the conservative protocol state required while recovering this
    /// durable journal state.
    ///
    /// `Intent` is safely `KnownNotApplied` because the ordering contract
    /// forbids the PTY call until a later `AcceptedNotDurable` record is
    /// synchronized. An accepted marker remains pending and must never be
    /// replayed after a crash.
    #[must_use]
    pub const fn recovery_protocol_state(self) -> InputEffectState {
        match self {
            Self::Intent | Self::KnownNotApplied => InputEffectState::KnownNotApplied,
            Self::AcceptedNotDurable => InputEffectState::AcceptedNotDurable,
            Self::DurableFull => InputEffectState::DurableFull,
            Self::DurablePrefix { applied_bytes } => {
                InputEffectState::DurablePrefix { applied_bytes }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GuardianInputDispositionTag {
    Intent,
    AcceptedNotDurable,
    DurableFull,
    DurablePrefix,
    KnownNotApplied,
}

impl GuardianInputDispositionTag {
    const fn to_wire(self) -> u8 {
        match self {
            Self::Intent => 1,
            Self::AcceptedNotDurable => 2,
            Self::DurableFull => 3,
            Self::DurablePrefix => 4,
            Self::KnownNotApplied => 5,
        }
    }

    fn from_wire(value: u8) -> Result<Self, GuardianInputJournalError> {
        match value {
            1 => Ok(Self::Intent),
            2 => Ok(Self::AcceptedNotDurable),
            3 => Ok(Self::DurableFull),
            4 => Ok(Self::DurablePrefix),
            5 => Ok(Self::KnownNotApplied),
            _ => Err(GuardianInputJournalError::InvalidDisposition { observed: value }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianInputJournalTail {
    Clean,
    Incomplete {
        committed_bytes: u64,
        trailing_bytes: u64,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianInputJournalReceipt {
    journal_sequence: u64,
    effect_id: Uuid,
    disposition: GuardianInputDisposition,
    committed_log_bytes: u64,
    record_digest: [u8; 32],
}

impl GuardianInputJournalReceipt {
    #[must_use]
    pub const fn journal_sequence(self) -> u64 {
        self.journal_sequence
    }

    #[must_use]
    pub const fn effect_id(self) -> Uuid {
        self.effect_id
    }

    #[must_use]
    pub const fn disposition(self) -> GuardianInputDisposition {
        self.disposition
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

impl std::fmt::Debug for GuardianInputJournalReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianInputJournalReceipt")
            .field("journal_sequence", &self.journal_sequence)
            .field("effect_id", &self.effect_id)
            .field("disposition", &self.disposition)
            .field("committed_log_bytes", &self.committed_log_bytes)
            .field("record_digest", &"[REDACTED]")
            .finish()
    }
}

/// Distinguishes a newly synchronized journal transition from reconciliation.
///
/// A caller may obtain a non-cloneable [`GuardianInputWritePermit`] only for a
/// newly synchronized `AcceptedNotDurable` transition in the one newly
/// admitted protocol transaction. A reconciled append never yields a permit;
/// it returns the current receipt so a retry cannot infer fresh authority from
/// an older intent or accepted-phase receipt.
#[derive(Debug, Eq, PartialEq)]
pub struct GuardianInputJournalAppend {
    identity: GuardianInputEffectIdentity,
    receipt: GuardianInputJournalReceipt,
    newly_committed: bool,
}

/// Opaque one-shot journal authority for the first PTY write attempt.
///
/// The future live PTY adapter must consume this value together with the newly
/// admitted protocol transaction. It is intentionally neither `Clone` nor
/// `Copy`, and external callers cannot construct it.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the guardian input write permit must be consumed by the one PTY write attempt"]
pub struct GuardianInputWritePermit {
    identity: GuardianInputEffectIdentity,
}

/// Opaque journal-to-protocol authority for one exact terminal disposition.
///
/// The applied prefix is carried inside the private receipt and therefore
/// cannot diverge between durable publication and protocol completion.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the terminal input permit must reconcile the exact durable disposition"]
pub struct GuardianInputTerminalPermit {
    identity: GuardianInputEffectIdentity,
    receipt: GuardianInputJournalReceipt,
}

impl GuardianInputJournalAppend {
    const fn committed(
        identity: GuardianInputEffectIdentity,
        receipt: GuardianInputJournalReceipt,
    ) -> Self {
        Self {
            identity,
            receipt,
            newly_committed: true,
        }
    }

    const fn reconciled(
        identity: GuardianInputEffectIdentity,
        receipt: GuardianInputJournalReceipt,
    ) -> Self {
        Self {
            identity,
            receipt,
            newly_committed: false,
        }
    }

    #[must_use]
    pub const fn receipt(&self) -> GuardianInputJournalReceipt {
        self.receipt
    }

    #[must_use]
    pub const fn is_newly_committed(&self) -> bool {
        self.newly_committed
    }

    #[must_use]
    pub const fn disposition(&self) -> GuardianInputDisposition {
        self.receipt().disposition()
    }

    #[must_use]
    pub const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    /// Consume the append result and return its one-shot PTY-write permit.
    ///
    /// The caller must also be executing the newly admitted protocol effect;
    /// a reconciled append can never authorize a PTY write.
    #[must_use]
    pub fn into_first_pty_write_permit(self) -> Option<GuardianInputWritePermit> {
        let Self {
            identity,
            receipt,
            newly_committed,
        } = self;
        (newly_committed
            && matches!(
                receipt.disposition,
                GuardianInputDisposition::AcceptedNotDurable
            ))
        .then_some(GuardianInputWritePermit { identity })
    }

    /// Consume the append result into an exact terminal protocol permit.
    ///
    /// Both newly committed and reconciled terminal records may complete the
    /// protocol idempotently. Intent and ambiguous acceptance never can.
    #[must_use]
    pub fn into_terminal_protocol_permit(self) -> Option<GuardianInputTerminalPermit> {
        let Self {
            identity,
            receipt,
            newly_committed: _,
        } = self;
        receipt
            .disposition
            .is_terminal()
            .then_some(GuardianInputTerminalPermit { identity, receipt })
    }
}

impl GuardianInputWritePermit {
    /// Exact authenticated identity that a consuming live writer must use.
    #[must_use]
    pub const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }
}

impl GuardianInputTerminalPermit {
    #[must_use]
    pub const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    #[must_use]
    pub const fn receipt(&self) -> GuardianInputJournalReceipt {
        self.receipt
    }

    #[must_use]
    pub const fn protocol_state(&self) -> InputEffectState {
        self.receipt.disposition.recovery_protocol_state()
    }

    /// Consume this exact durable result and complete the protocol once.
    pub fn reconcile_protocol(
        self,
        protocol: &mut GuardianProtocolState,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        match self.receipt.disposition {
            GuardianInputDisposition::DurableFull => {
                protocol.mark_input_durable_full(self.identity)
            }
            GuardianInputDisposition::DurablePrefix { applied_bytes } => {
                protocol.mark_input_durable_prefix(self.identity, applied_bytes)
            }
            GuardianInputDisposition::KnownNotApplied => {
                protocol.mark_input_known_not_applied(self.identity)
            }
            GuardianInputDisposition::Intent | GuardianInputDisposition::AcceptedNotDurable => {
                Err(GuardianProtocolError::StateInvariantViolation(
                    "input-terminal-permit-disposition",
                ))
            }
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RecoveredGuardianInputEffect {
    identity: GuardianInputEffectIdentity,
    input_bytes: u32,
    disposition: GuardianInputDisposition,
    intent_receipt: GuardianInputJournalReceipt,
    accepted_receipt: Option<GuardianInputJournalReceipt>,
    terminal_receipt: Option<GuardianInputJournalReceipt>,
}

impl RecoveredGuardianInputEffect {
    #[must_use]
    pub const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    #[must_use]
    pub const fn input_bytes(&self) -> u32 {
        self.input_bytes
    }

    #[must_use]
    pub const fn disposition(&self) -> GuardianInputDisposition {
        self.disposition
    }

    #[must_use]
    pub const fn receipt(&self) -> GuardianInputJournalReceipt {
        match self.terminal_receipt {
            Some(receipt) => receipt,
            None => match self.accepted_receipt {
                Some(receipt) => receipt,
                None => self.intent_receipt,
            },
        }
    }

    #[must_use]
    pub fn receipt_for(
        &self,
        disposition: GuardianInputDisposition,
    ) -> Option<GuardianInputJournalReceipt> {
        match disposition {
            GuardianInputDisposition::Intent => Some(self.intent_receipt),
            GuardianInputDisposition::AcceptedNotDurable => self.accepted_receipt,
            GuardianInputDisposition::DurableFull
            | GuardianInputDisposition::DurablePrefix { .. }
            | GuardianInputDisposition::KnownNotApplied => {
                if self.disposition == disposition {
                    self.terminal_receipt
                } else {
                    None
                }
            }
        }
    }
}

impl std::fmt::Debug for RecoveredGuardianInputEffect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveredGuardianInputEffect")
            .field("identity", &self.identity)
            .field("input_bytes", &self.input_bytes)
            .field("disposition", &self.disposition)
            .field("receipt", &self.receipt())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianInputJournalError {
    #[error("guardian input journal limits are invalid")]
    InvalidLimits,
    #[error("guardian input effect identity is invalid")]
    InvalidIdentity,
    #[error("guardian input byte length must be nonzero and within the configured limit")]
    InvalidInputLength,
    #[error("guardian durable input prefix length is invalid for the authenticated input length")]
    InvalidAppliedByteCount,
    #[error("guardian input journal arithmetic overflow")]
    ArithmeticOverflow,
    #[error("guardian input journal sequence space is exhausted")]
    JournalSequenceExhausted,
    #[error("guardian input journal descriptor is not a regular file")]
    NotRegularFile,
    #[error("guardian input journal parent descriptor is not a directory")]
    NotDirectory,
    #[error("guardian input journal file header is torn: found {actual} of {expected} bytes")]
    TornFileHeader { expected: usize, actual: u64 },
    #[error("guardian input journal file magic is invalid")]
    InvalidFileMagic,
    #[error(
        "guardian input journal v1 has an ambiguous fieldless durable disposition and requires conservative offline migration"
    )]
    LegacyV1DispositionAmbiguous,
    #[error("unsupported guardian input journal version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("guardian input journal file header length is invalid: {observed}")]
    InvalidFileHeaderLength { observed: u32 },
    #[error("guardian input journal belongs to another durable pane")]
    PaneIdentityMismatch,
    #[error("guardian input journal encryption key identity does not match")]
    KeyIdentityMismatch,
    #[error("guardian input journal file header is noncanonical")]
    NonCanonicalFileHeader,
    #[error("guardian input journal file header digest mismatch")]
    FileHeaderDigestMismatch,
    #[error("guardian input journal record at byte {offset} has invalid magic")]
    InvalidRecordMagic { offset: u64 },
    #[error("guardian input journal record at byte {offset} has invalid length {observed}")]
    InvalidRecordLength { offset: u64, observed: u32 },
    #[error("guardian input journal record at byte {offset} is noncanonical")]
    NonCanonicalRecord { offset: u64 },
    #[error("guardian input disposition value {observed} is invalid")]
    InvalidDisposition { observed: u8 },
    #[error("guardian input journal sequence mismatch: expected {expected}, observed {observed}")]
    JournalSequenceMismatch { expected: u64, observed: u64 },
    #[error("guardian input journal record digest mismatch at sequence {sequence}")]
    RecordDigestMismatch { sequence: u64 },
    #[error("guardian input journal record authentication failed")]
    RecordAuthenticationFailed,
    #[error("guardian input journal encryption failed")]
    Encryption(#[source] GuardianOutputJournalError),
    #[error("guardian input journal digest chain mismatch at sequence {sequence}")]
    RecordChainMismatch { sequence: u64 },
    #[error("guardian input effect identity conflicts with an existing effect")]
    EffectIdentityConflict,
    #[error("guardian input effect transition from {from:?} to {to:?} is invalid")]
    InvalidTransition {
        from: GuardianInputDisposition,
        to: GuardianInputDisposition,
    },
    #[error("guardian input effects are not monotonic within their lease generation")]
    NonMonotonicInputIdentity,
    #[error("guardian input journal record limit {maximum} is exhausted")]
    RecordLimit { maximum: u64 },
    #[error("guardian input journal effect limit {maximum} is exhausted")]
    EffectLimit { maximum: usize },
    #[error("guardian input journal exceeds its byte limit: {observed} > {maximum}")]
    LogByteLimit { observed: u64, maximum: u64 },
    #[error("new guardian input journal is not active until its parent directory is synchronized")]
    DirectoryEntryNotDurable,
    #[error("guardian input journal has an incomplete tail and must be sealed")]
    IncompleteTail,
    #[error("guardian input journal is poisoned after an ambiguous write or sync failure")]
    Poisoned,
    #[error("guardian input journal length changed outside its exclusive owner: expected {expected}, observed {observed}")]
    ExternalLengthChange { expected: u64, observed: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct JournalScan {
    committed_bytes: u64,
    record_count: u64,
    next_journal_sequence: Option<u64>,
    terminal_record_digest: [u8; 32],
    tail: GuardianInputJournalTail,
    effects: BTreeMap<Uuid, RecoveredGuardianInputEffect>,
    last_input_identity: Option<(u64, Uuid, u64)>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InjectedAppendFault {
    BeforeWrite,
    AfterHeader,
    AfterCiphertext,
    BeforeSync,
    AfterSync,
}

/// Exclusive append and recovery authority for one durable pane's input log.
pub struct GuardianInputJournal {
    file: File,
    durable_pane_id: Uuid,
    cipher: GuardianOutputCipher,
    limits: GuardianInputJournalLimits,
    committed_bytes: u64,
    record_count: u64,
    next_journal_sequence: Option<u64>,
    terminal_record_digest: [u8; 32],
    tail: GuardianInputJournalTail,
    effects: BTreeMap<Uuid, RecoveredGuardianInputEffect>,
    last_input_identity: Option<(u64, Uuid, u64)>,
    directory_entry_sync_required: bool,
    poisoned: bool,
    #[cfg(test)]
    injected_append_fault: Option<InjectedAppendFault>,
}

impl GuardianInputJournal {
    pub fn open(
        mut file: File,
        durable_pane_id: Uuid,
        cipher: GuardianOutputCipher,
        limits: GuardianInputJournalLimits,
    ) -> Result<Self, GuardianInputJournalError> {
        if durable_pane_id.is_nil() {
            return Err(GuardianInputJournalError::InvalidIdentity);
        }
        let limits = limits.validate()?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianInputJournalError::NotRegularFile);
        }
        let mut physical_bytes = metadata.len();
        if physical_bytes > limits.max_log_bytes {
            return Err(GuardianInputJournalError::LogByteLimit {
                observed: physical_bytes,
                maximum: limits.max_log_bytes,
            });
        }
        let initialized = physical_bytes == 0;
        if initialized {
            let header = encode_file_header(durable_pane_id, cipher.key_id());
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&header)?;
            file.sync_all()?;
            physical_bytes = FILE_HEADER_BYTES_U64;
        }
        if physical_bytes < FILE_HEADER_BYTES_U64 {
            return Err(GuardianInputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual: physical_bytes,
            });
        }
        let scan = scan_journal(
            &mut file,
            physical_bytes,
            durable_pane_id,
            &cipher,
            limits,
        )?;
        Ok(Self {
            file,
            durable_pane_id,
            cipher,
            limits,
            committed_bytes: scan.committed_bytes,
            record_count: scan.record_count,
            next_journal_sequence: scan.next_journal_sequence,
            terminal_record_digest: scan.terminal_record_digest,
            tail: scan.tail,
            effects: scan.effects,
            last_input_identity: scan.last_input_identity,
            directory_entry_sync_required: initialized,
            poisoned: false,
            #[cfg(test)]
            injected_append_fault: None,
        })
    }

    #[must_use]
    pub const fn durable_pane_id(&self) -> Uuid {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn tail(&self) -> GuardianInputJournalTail {
        self.tail
    }

    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    #[must_use]
    pub const fn directory_entry_sync_required(&self) -> bool {
        self.directory_entry_sync_required
    }

    #[must_use]
    pub fn effect(&self, effect_id: Uuid) -> Option<&RecoveredGuardianInputEffect> {
        self.effects.get(&effect_id)
    }

    #[must_use]
    pub fn effects(&self) -> impl ExactSizeIterator<Item = &RecoveredGuardianInputEffect> {
        self.effects.values()
    }

    pub fn sync_parent_directory_and_activate(
        &mut self,
        parent_directory: &File,
    ) -> Result<(), GuardianInputJournalError> {
        if !self.directory_entry_sync_required {
            return Ok(());
        }
        if !parent_directory.metadata()?.file_type().is_dir() {
            return Err(GuardianInputJournalError::NotDirectory);
        }
        parent_directory.sync_all()?;
        self.directory_entry_sync_required = false;
        Ok(())
    }

    #[cfg(test)]
    fn inject_append_fault(&mut self, fault: InjectedAppendFault) {
        self.injected_append_fault = Some(fault);
    }

    #[cfg(test)]
    fn fail_append_if_injected(
        &mut self,
        stage: InjectedAppendFault,
    ) -> std::io::Result<()> {
        if self.injected_append_fault == Some(stage) {
            self.injected_append_fault = None;
            Err(std::io::Error::other("injected guardian input append fault"))
        } else {
            Ok(())
        }
    }

    /// Synchronize a new intent before any input bytes are forwarded.
    pub fn append_intent_and_sync(
        &mut self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<GuardianInputJournalAppend, GuardianInputJournalError> {
        self.validate_identity(identity)?;
        if let Some(existing) = self.effects.get(&identity.effect_id()) {
            if existing.identity == identity && existing.input_bytes == identity.input_bytes() {
                return Ok(GuardianInputJournalAppend::reconciled(
                    existing.identity,
                    existing.receipt(),
                ));
            }
            return Err(GuardianInputJournalError::EffectIdentityConflict);
        }
        if self.effects.len() >= self.limits.max_effects {
            return Err(GuardianInputJournalError::EffectLimit {
                maximum: self.limits.max_effects,
            });
        }
        validate_monotonic_input(self.last_input_identity, identity)?;
        self.append_record(identity, GuardianInputDisposition::Intent)
            .map(|receipt| GuardianInputJournalAppend::committed(identity, receipt))
    }

    /// Synchronize one exact state transition for an already durable intent.
    pub fn append_disposition_and_sync(
        &mut self,
        identity: GuardianInputEffectIdentity,
        disposition: GuardianInputDisposition,
    ) -> Result<GuardianInputJournalAppend, GuardianInputJournalError> {
        if disposition == GuardianInputDisposition::Intent {
            return Err(GuardianInputJournalError::InvalidTransition {
                from: GuardianInputDisposition::Intent,
                to: disposition,
            });
        }
        let existing = self
            .effects
            .get(&identity.effect_id())
            .cloned()
            .ok_or(GuardianInputJournalError::EffectIdentityConflict)?;
        if existing.identity != identity {
            return Err(GuardianInputJournalError::EffectIdentityConflict);
        }
        disposition.validate_for_input_bytes(existing.input_bytes)?;
        if existing.disposition == disposition
            || (existing.disposition.is_terminal()
                && disposition == GuardianInputDisposition::AcceptedNotDurable)
        {
            return Ok(GuardianInputJournalAppend::reconciled(
                existing.identity,
                existing.receipt(),
            ));
        }
        validate_transition(existing.disposition, disposition)?;
        self.append_record(identity, disposition)
            .map(|receipt| GuardianInputJournalAppend::committed(identity, receipt))
    }

    fn validate_identity(
        &self,
        identity: GuardianInputEffectIdentity,
    ) -> Result<(), GuardianInputJournalError> {
        if identity.pane_id() != self.durable_pane_id {
            return Err(GuardianInputJournalError::PaneIdentityMismatch);
        }
        let input_bytes = identity.input_bytes();
        if input_bytes == 0 || input_bytes > self.limits.max_input_bytes {
            return Err(GuardianInputJournalError::InvalidInputLength);
        }
        Ok(())
    }

    fn append_record(
        &mut self,
        identity: GuardianInputEffectIdentity,
        disposition: GuardianInputDisposition,
    ) -> Result<GuardianInputJournalReceipt, GuardianInputJournalError> {
        if self.poisoned {
            return Err(GuardianInputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required {
            return Err(GuardianInputJournalError::DirectoryEntryNotDurable);
        }
        if self.tail != GuardianInputJournalTail::Clean {
            return Err(GuardianInputJournalError::IncompleteTail);
        }
        if self.record_count >= self.limits.max_records {
            return Err(GuardianInputJournalError::RecordLimit {
                maximum: self.limits.max_records,
            });
        }
        let journal_sequence = self
            .next_journal_sequence
            .ok_or(GuardianInputJournalError::JournalSequenceExhausted)?;
        let projected_bytes = self
            .committed_bytes
            .checked_add(RECORD_BYTES_U64)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        if projected_bytes > self.limits.max_log_bytes {
            return Err(GuardianInputJournalError::LogByteLimit {
                observed: projected_bytes,
                maximum: self.limits.max_log_bytes,
            });
        }
        let physical_bytes = self.file.metadata()?.len();
        if physical_bytes != self.committed_bytes {
            self.poisoned = true;
            return Err(GuardianInputJournalError::ExternalLengthChange {
                expected: self.committed_bytes,
                observed: physical_bytes,
            });
        }

        let input_bytes = identity.input_bytes();
        disposition.validate_for_input_bytes(input_bytes)?;
        let plaintext = encode_plaintext(identity, disposition, self.terminal_record_digest);
        let header_prefix = encode_record_header_prefix(journal_sequence, disposition);
        let aad = record_aad(self.durable_pane_id, &header_prefix);
        let (nonce, ciphertext) = self
            .cipher
            .seal_guardian_metadata(&plaintext, &aad)
            .map_err(GuardianInputJournalError::Encryption)?;
        if ciphertext.len() != RECORD_CIPHERTEXT_BYTES {
            return Err(GuardianInputJournalError::ArithmeticOverflow);
        }
        let record_digest = record_digest(
            self.durable_pane_id,
            &header_prefix,
            &nonce,
            &ciphertext,
        );
        let header = encode_record_header(header_prefix, nonce, record_digest);
        let receipt = GuardianInputJournalReceipt {
            journal_sequence,
            effect_id: identity.effect_id(),
            disposition,
            committed_log_bytes: projected_bytes,
            record_digest,
        };
        let recovered = advance_effect(
            self.effects.get(&identity.effect_id()).cloned(),
            identity,
            input_bytes,
            disposition,
            receipt,
        )?;
        let next_record_count = self
            .record_count
            .checked_add(1)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        let result = (|| -> std::io::Result<()> {
            #[cfg(test)]
            self.fail_append_if_injected(InjectedAppendFault::BeforeWrite)?;
            self.file.seek(SeekFrom::Start(self.committed_bytes))?;
            self.file.write_all(&header)?;
            #[cfg(test)]
            self.fail_append_if_injected(InjectedAppendFault::AfterHeader)?;
            self.file.write_all(&ciphertext)?;
            #[cfg(test)]
            self.fail_append_if_injected(InjectedAppendFault::AfterCiphertext)?;
            #[cfg(test)]
            self.fail_append_if_injected(InjectedAppendFault::BeforeSync)?;
            self.file.sync_all()?;
            #[cfg(test)]
            self.fail_append_if_injected(InjectedAppendFault::AfterSync)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.poisoned = true;
            return Err(GuardianInputJournalError::Io(error));
        }
        self.committed_bytes = projected_bytes;
        self.record_count = next_record_count;
        self.next_journal_sequence = journal_sequence.checked_add(1);
        self.terminal_record_digest = record_digest;
        if disposition == GuardianInputDisposition::Intent {
            self.last_input_identity = Some((
                identity.generation(),
                identity.mux_incarnation(),
                identity.sequence(),
            ));
        }
        self.effects.insert(identity.effect_id(), recovered);
        Ok(receipt)
    }
}

fn validate_monotonic_input(
    previous: Option<(u64, Uuid, u64)>,
    identity: GuardianInputEffectIdentity,
) -> Result<(), GuardianInputJournalError> {
    let Some((previous_generation, previous_mux, previous_sequence)) = previous else {
        return Ok(());
    };
    if identity.generation() < previous_generation
        || (identity.generation() == previous_generation
            && (identity.mux_incarnation() != previous_mux
                || identity.sequence() <= previous_sequence))
    {
        return Err(GuardianInputJournalError::NonMonotonicInputIdentity);
    }
    Ok(())
}

fn validate_transition(
    from: GuardianInputDisposition,
    to: GuardianInputDisposition,
) -> Result<(), GuardianInputJournalError> {
    let valid = matches!(
        (from, to),
        (
            GuardianInputDisposition::Intent,
            GuardianInputDisposition::AcceptedNotDurable
        ) | (
            GuardianInputDisposition::Intent,
            GuardianInputDisposition::KnownNotApplied
        ) | (
            GuardianInputDisposition::AcceptedNotDurable,
            GuardianInputDisposition::DurableFull
                | GuardianInputDisposition::DurablePrefix { .. }
        ) | (
            GuardianInputDisposition::AcceptedNotDurable,
            GuardianInputDisposition::KnownNotApplied
        )
    );
    if valid {
        Ok(())
    } else {
        Err(GuardianInputJournalError::InvalidTransition { from, to })
    }
}

fn advance_effect(
    previous: Option<RecoveredGuardianInputEffect>,
    identity: GuardianInputEffectIdentity,
    input_bytes: u32,
    disposition: GuardianInputDisposition,
    receipt: GuardianInputJournalReceipt,
) -> Result<RecoveredGuardianInputEffect, GuardianInputJournalError> {
    match (previous, disposition) {
        (None, GuardianInputDisposition::Intent) => Ok(RecoveredGuardianInputEffect {
            identity,
            input_bytes,
            disposition,
            intent_receipt: receipt,
            accepted_receipt: None,
            terminal_receipt: None,
        }),
        (Some(mut effect), GuardianInputDisposition::AcceptedNotDurable) => {
            effect.disposition = disposition;
            effect.accepted_receipt = Some(receipt);
            Ok(effect)
        }
        (
            Some(mut effect),
            GuardianInputDisposition::DurableFull
            | GuardianInputDisposition::DurablePrefix { .. }
            | GuardianInputDisposition::KnownNotApplied,
        ) => {
            effect.disposition = disposition;
            effect.terminal_receipt = Some(receipt);
            Ok(effect)
        }
        _ => Err(GuardianInputJournalError::EffectIdentityConflict),
    }
}

fn encode_file_header(durable_pane_id: Uuid, key_id: [u8; KEY_ID_BYTES]) -> [u8; FILE_HEADER_BYTES] {
    let mut header = [0_u8; FILE_HEADER_BYTES];
    header[0..8].copy_from_slice(&FILE_MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&FILE_HEADER_BYTES_U32.to_le_bytes());
    header[16..32].copy_from_slice(durable_pane_id.as_bytes());
    header[32..40].copy_from_slice(&key_id);
    let mut hasher = Sha256::new();
    hasher.update(FILE_DIGEST_DOMAIN);
    hasher.update(&header[..96]);
    header[96..128].copy_from_slice(&hasher.finalize());
    header
}

fn validate_file_header(
    header: &[u8; FILE_HEADER_BYTES],
    durable_pane_id: Uuid,
    key_id: [u8; KEY_ID_BYTES],
) -> Result<(), GuardianInputJournalError> {
    if header[0..8] == LEGACY_FILE_MAGIC_V1 {
        return Err(GuardianInputJournalError::LegacyV1DispositionAmbiguous);
    }
    if header[0..8] != FILE_MAGIC {
        return Err(GuardianInputJournalError::InvalidFileMagic);
    }
    let version = read_u32(&header[8..12]);
    if version != FORMAT_VERSION {
        return Err(GuardianInputJournalError::UnsupportedVersion { observed: version });
    }
    let header_bytes = read_u32(&header[12..16]);
    if header_bytes != FILE_HEADER_BYTES_U32 {
        return Err(GuardianInputJournalError::InvalidFileHeaderLength {
            observed: header_bytes,
        });
    }
    if header[16..32] != *durable_pane_id.as_bytes() {
        return Err(GuardianInputJournalError::PaneIdentityMismatch);
    }
    if header[32..40] != key_id {
        return Err(GuardianInputJournalError::KeyIdentityMismatch);
    }
    if header[40..96].iter().any(|byte| *byte != 0) {
        return Err(GuardianInputJournalError::NonCanonicalFileHeader);
    }
    let mut hasher = Sha256::new();
    hasher.update(FILE_DIGEST_DOMAIN);
    hasher.update(&header[..96]);
    if header[96..128] != hasher.finalize()[..] {
        return Err(GuardianInputJournalError::FileHeaderDigestMismatch);
    }
    Ok(())
}

fn encode_record_header_prefix(
    journal_sequence: u64,
    disposition: GuardianInputDisposition,
) -> [u8; 32] {
    let mut prefix = [0_u8; 32];
    prefix[0..8].copy_from_slice(&RECORD_MAGIC);
    prefix[8..12].copy_from_slice(&RECORD_HEADER_BYTES_U32.to_le_bytes());
    prefix[12] = disposition.tag().to_wire();
    prefix[16..24].copy_from_slice(&journal_sequence.to_le_bytes());
    prefix[24..28].copy_from_slice(&RECORD_PLAINTEXT_BYTES_U32.to_le_bytes());
    prefix[28..32].copy_from_slice(&RECORD_CIPHERTEXT_BYTES_U32.to_le_bytes());
    prefix
}

fn encode_record_header(
    prefix: [u8; 32],
    nonce: [u8; NONCE_BYTES],
    digest: [u8; 32],
) -> [u8; RECORD_HEADER_BYTES] {
    let mut header = [0_u8; RECORD_HEADER_BYTES];
    header[..32].copy_from_slice(&prefix);
    header[32..56].copy_from_slice(&nonce);
    header[56..88].copy_from_slice(&digest);
    header
}

fn encode_plaintext(
    identity: GuardianInputEffectIdentity,
    disposition: GuardianInputDisposition,
    previous_digest: [u8; 32],
) -> [u8; RECORD_PLAINTEXT_BYTES] {
    let mut plaintext = [0_u8; RECORD_PLAINTEXT_BYTES];
    plaintext[0..16].copy_from_slice(identity.mux_incarnation().as_bytes());
    plaintext[16..24].copy_from_slice(&identity.generation().to_le_bytes());
    plaintext[24..32].copy_from_slice(&identity.sequence().to_le_bytes());
    plaintext[32..48].copy_from_slice(identity.effect_id().as_bytes());
    plaintext[48..80].copy_from_slice(&identity.payload_sha256());
    plaintext[80..84].copy_from_slice(&identity.input_bytes().to_le_bytes());
    plaintext[84..88].copy_from_slice(&disposition.stored_applied_bytes().to_le_bytes());
    plaintext[104..136].copy_from_slice(&previous_digest);
    plaintext
}

fn record_aad(durable_pane_id: Uuid, prefix: &[u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RECORD_AAD_DOMAIN.len() + 16 + prefix.len());
    aad.extend_from_slice(RECORD_AAD_DOMAIN);
    aad.extend_from_slice(durable_pane_id.as_bytes());
    aad.extend_from_slice(prefix);
    aad
}

fn record_digest(
    durable_pane_id: Uuid,
    prefix: &[u8; 32],
    nonce: &[u8; NONCE_BYTES],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(durable_pane_id.as_bytes());
    hasher.update(prefix);
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().into()
}

fn scan_journal(
    file: &mut File,
    physical_bytes: u64,
    durable_pane_id: Uuid,
    cipher: &GuardianOutputCipher,
    limits: GuardianInputJournalLimits,
) -> Result<JournalScan, GuardianInputJournalError> {
    file.seek(SeekFrom::Start(0))?;
    let mut file_header = [0_u8; FILE_HEADER_BYTES];
    file.read_exact(&mut file_header)?;
    validate_file_header(&file_header, durable_pane_id, cipher.key_id())?;

    let mut committed_bytes = FILE_HEADER_BYTES_U64;
    let mut record_count = 0_u64;
    let mut next_journal_sequence = Some(1_u64);
    let mut terminal_record_digest = [0_u8; 32];
    let mut effects: BTreeMap<Uuid, RecoveredGuardianInputEffect> = BTreeMap::new();
    let mut last_input_identity = None;
    while committed_bytes < physical_bytes {
        let remaining = physical_bytes
            .checked_sub(committed_bytes)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        if remaining < RECORD_BYTES_U64 {
            return Ok(JournalScan {
                committed_bytes,
                record_count,
                next_journal_sequence,
                terminal_record_digest,
                tail: GuardianInputJournalTail::Incomplete {
                    committed_bytes,
                    trailing_bytes: remaining,
                },
                effects,
                last_input_identity,
            });
        }
        if record_count >= limits.max_records {
            return Err(GuardianInputJournalError::RecordLimit {
                maximum: limits.max_records,
            });
        }
        file.seek(SeekFrom::Start(committed_bytes))?;
        let mut header = [0_u8; RECORD_HEADER_BYTES];
        file.read_exact(&mut header)?;
        if header[0..8] != RECORD_MAGIC {
            return Err(GuardianInputJournalError::InvalidRecordMagic {
                offset: committed_bytes,
            });
        }
        let header_bytes = read_u32(&header[8..12]);
        if header_bytes != RECORD_HEADER_BYTES_U32 {
            return Err(GuardianInputJournalError::InvalidRecordLength {
                offset: committed_bytes,
                observed: header_bytes,
            });
        }
        if header[13..16].iter().any(|byte| *byte != 0)
            || header[88..96].iter().any(|byte| *byte != 0)
            || read_u32(&header[24..28]) != RECORD_PLAINTEXT_BYTES_U32
            || read_u32(&header[28..32]) != RECORD_CIPHERTEXT_BYTES_U32
        {
            return Err(GuardianInputJournalError::NonCanonicalRecord {
                offset: committed_bytes,
            });
        }
        let disposition_tag = GuardianInputDispositionTag::from_wire(header[12])?;
        let journal_sequence = read_u64(&header[16..24]);
        let expected_sequence = next_journal_sequence
            .ok_or(GuardianInputJournalError::JournalSequenceExhausted)?;
        if journal_sequence != expected_sequence {
            return Err(GuardianInputJournalError::JournalSequenceMismatch {
                expected: expected_sequence,
                observed: journal_sequence,
            });
        }
        let mut ciphertext = [0_u8; RECORD_CIPHERTEXT_BYTES];
        file.read_exact(&mut ciphertext)?;
        let mut prefix = [0_u8; 32];
        prefix.copy_from_slice(&header[..32]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&header[32..56]);
        let expected_digest = record_digest(durable_pane_id, &prefix, &nonce, &ciphertext);
        if header[56..88] != expected_digest {
            return Err(GuardianInputJournalError::RecordDigestMismatch {
                sequence: journal_sequence,
            });
        }
        let aad = record_aad(durable_pane_id, &prefix);
        let plaintext = cipher
            .open_guardian_metadata(&nonce, &ciphertext, &aad)
            .map_err(|_| GuardianInputJournalError::RecordAuthenticationFailed)?;
        if plaintext.len() != RECORD_PLAINTEXT_BYTES || plaintext[88..104].iter().any(|b| *b != 0)
        {
            return Err(GuardianInputJournalError::NonCanonicalRecord {
                offset: committed_bytes,
            });
        }
        if plaintext[104..136] != terminal_record_digest {
            return Err(GuardianInputJournalError::RecordChainMismatch {
                sequence: journal_sequence,
            });
        }
        let input_bytes = read_u32(&plaintext[80..84]);
        if input_bytes == 0 || input_bytes > limits.max_input_bytes {
            return Err(GuardianInputJournalError::InvalidInputLength);
        }
        let disposition = GuardianInputDisposition::from_stored(
            disposition_tag,
            read_u32(&plaintext[84..88]),
            input_bytes,
        )?;
        let identity = decode_identity(durable_pane_id, input_bytes, &plaintext)?;
        let existing = effects.get(&identity.effect_id()).cloned();
        match (existing.as_ref(), disposition) {
            (None, GuardianInputDisposition::Intent) => {
                if effects.len() >= limits.max_effects {
                    return Err(GuardianInputJournalError::EffectLimit {
                        maximum: limits.max_effects,
                    });
                }
                validate_monotonic_input(last_input_identity, identity)?;
                last_input_identity = Some((
                    identity.generation(),
                    identity.mux_incarnation(),
                    identity.sequence(),
                ));
            }
            (Some(previous), next)
                if previous.identity == identity && previous.input_bytes == input_bytes =>
            {
                validate_transition(previous.disposition, next)?;
            }
            _ => return Err(GuardianInputJournalError::EffectIdentityConflict),
        }
        let projected_bytes = committed_bytes
            .checked_add(RECORD_BYTES_U64)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        let receipt = GuardianInputJournalReceipt {
            journal_sequence,
            effect_id: identity.effect_id(),
            disposition,
            committed_log_bytes: projected_bytes,
            record_digest: expected_digest,
        };
        let recovered = advance_effect(existing, identity, input_bytes, disposition, receipt)?;
        effects.insert(identity.effect_id(), recovered);
        committed_bytes = projected_bytes;
        record_count = record_count
            .checked_add(1)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        next_journal_sequence = journal_sequence.checked_add(1);
        terminal_record_digest = expected_digest;
    }
    Ok(JournalScan {
        committed_bytes,
        record_count,
        next_journal_sequence,
        terminal_record_digest,
        tail: GuardianInputJournalTail::Clean,
        effects,
        last_input_identity,
    })
}

fn decode_identity(
    durable_pane_id: Uuid,
    input_bytes: u32,
    plaintext: &[u8],
) -> Result<GuardianInputEffectIdentity, GuardianInputJournalError> {
    let mux_incarnation = Uuid::from_slice(&plaintext[0..16])
        .map_err(|_| GuardianInputJournalError::InvalidIdentity)?;
    let effect_id = Uuid::from_slice(&plaintext[32..48])
        .map_err(|_| GuardianInputJournalError::InvalidIdentity)?;
    let mut payload_sha256 = [0_u8; 32];
    payload_sha256.copy_from_slice(&plaintext[48..80]);
    GuardianInputEffectIdentity::new(
        durable_pane_id,
        mux_incarnation,
        read_u64(&plaintext[16..24]),
        read_u64(&plaintext[24..32]),
        effect_id,
        input_bytes,
        payload_sha256,
    )
    .map_err(|_| GuardianInputJournalError::InvalidIdentity)
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut fixed = [0_u8; 4];
    fixed.copy_from_slice(bytes);
    u32::from_le_bytes(fixed)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut fixed = [0_u8; 8];
    fixed.copy_from_slice(bytes);
    u64::from_le_bytes(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom as _;
    use std::io::Write as _;

    fn pane() -> Uuid {
        Uuid::from_bytes([0x41; 16])
    }

    fn cipher() -> GuardianOutputCipher {
        GuardianOutputCipher::try_from_key_slice(&[0x73; 32]).expect("valid fixture key")
    }

    fn identity(sequence: u64, effect_byte: u8, payload_byte: u8) -> GuardianInputEffectIdentity {
        identity_with_input_bytes(sequence, effect_byte, payload_byte, 4)
    }

    fn identity_with_input_bytes(
        sequence: u64,
        effect_byte: u8,
        payload_byte: u8,
        input_bytes: u32,
    ) -> GuardianInputEffectIdentity {
        GuardianInputEffectIdentity::new(
            pane(),
            Uuid::from_bytes([0x52; 16]),
            3,
            sequence,
            Uuid::from_bytes([effect_byte; 16]),
            input_bytes,
            [payload_byte; 32],
        )
        .expect("valid fixture identity")
    }

    fn assert_reconciled(
        append: GuardianInputJournalAppend,
        expected_receipt: GuardianInputJournalReceipt,
    ) {
        assert!(!append.is_newly_committed());
        let identity = append.identity();
        assert_eq!(append.receipt(), expected_receipt);
        if expected_receipt.disposition().is_terminal() {
            let permit = append
                .into_terminal_protocol_permit()
                .expect("terminal reconciliation yields exact protocol permit");
            assert_eq!(permit.identity(), identity);
            assert_eq!(permit.receipt(), expected_receipt);
            assert_eq!(
                permit.protocol_state(),
                expected_receipt.disposition().recovery_protocol_state()
            );
        } else {
            assert!(append.into_first_pty_write_permit().is_none());
        }
    }

    #[test]
    fn recovery_mapping_never_replays_an_accepted_input() {
        let default_limits = GuardianInputJournalLimits::default();
        assert_eq!(
            default_limits.max_input_bytes,
            u32::try_from(GUARDIAN_MAX_INPUT_BYTES).unwrap()
        );
        assert!(matches!(
            GuardianInputJournalLimits {
                max_input_bytes: default_limits.max_input_bytes + 1,
                ..default_limits
            }
            .validate(),
            Err(GuardianInputJournalError::InvalidLimits)
        ));
        assert_eq!(
            GuardianInputDisposition::Intent.recovery_protocol_state(),
            InputEffectState::KnownNotApplied
        );
        assert_eq!(
            GuardianInputDisposition::KnownNotApplied.recovery_protocol_state(),
            InputEffectState::KnownNotApplied
        );
        assert_eq!(
            GuardianInputDisposition::AcceptedNotDurable.recovery_protocol_state(),
            InputEffectState::AcceptedNotDurable
        );
        assert_eq!(
            GuardianInputDisposition::DurableFull.recovery_protocol_state(),
            InputEffectState::DurableFull
        );
        assert_eq!(
            GuardianInputDisposition::DurablePrefix { applied_bytes: 2 }
                .recovery_protocol_state(),
            InputEffectState::DurablePrefix { applied_bytes: 2 }
        );
    }

    #[cfg(unix)]
    fn create_file(path: &std::path::Path) -> File {
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("create private input journal")
    }

    #[cfg(unix)]
    fn open_file(path: &std::path::Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("reopen input journal")
    }

    #[cfg(unix)]
    fn open_directory(path: &std::path::Path) -> File {
        File::open(path).expect("open parent directory")
    }

    #[cfg(unix)]
    #[test]
    fn exact_lifecycle_survives_reopen_without_plaintext_fingerprint() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x61, 0xa7);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        let intent_append = journal
            .append_intent_and_sync(input)
            .expect("persist intent");
        assert!(intent_append.is_newly_committed());
        let intent_receipt = intent_append.receipt();
        assert!(intent_append.into_first_pty_write_permit().is_none());
        let accepted_append = journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("persist accepted marker");
        assert!(accepted_append.is_newly_committed());
        let accepted_receipt = accepted_append.receipt();
        let write_permit = accepted_append
            .into_first_pty_write_permit()
            .expect("fresh accepted marker yields one bound write permit");
        assert_eq!(write_permit.identity(), input);
        let accepted_retry = journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("reconcile accepted marker acknowledgement loss");
        assert_reconciled(accepted_retry, accepted_receipt);
        let durable_append = journal
            .append_disposition_and_sync(input, GuardianInputDisposition::DurableFull)
            .expect("persist durable marker");
        assert!(durable_append.is_newly_committed());
        let durable = durable_append.receipt();
        let durable_permit = durable_append
            .into_terminal_protocol_permit()
            .expect("durable full yields exact protocol permit");
        assert_eq!(durable_permit.identity(), input);
        assert_eq!(durable_permit.receipt(), durable);
        assert_eq!(durable_permit.protocol_state(), InputEffectState::DurableFull);
        drop(journal);

        let physical = std::fs::read(&path).expect("read physical bytes");
        assert!(
            !physical
                .windows(32)
                .any(|window| window == [0xa7; 32].as_slice())
        );
        assert!(
            !physical
                .windows(16)
                .any(|window| window == [0x61; 16].as_slice())
        );

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover journal");
        {
            let recovered = reopened.effect(input.effect_id()).expect("effect recovered");
            assert_eq!(recovered.identity(), input);
            assert_eq!(recovered.disposition(), GuardianInputDisposition::DurableFull);
            assert_eq!(recovered.receipt(), durable);
            assert_eq!(
                recovered.receipt_for(GuardianInputDisposition::Intent),
                Some(intent_receipt)
            );
            assert_eq!(
                recovered.receipt_for(GuardianInputDisposition::AcceptedNotDurable),
                Some(accepted_receipt)
            );
            assert!(!format!("{recovered:?}").contains(&"a7".repeat(32)));
        }
        assert_reconciled(
            reopened
                .append_intent_and_sync(input)
                .expect("reconcile original intent"),
            durable,
        );
        assert_reconciled(
            reopened
                .append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                )
                .expect("reconcile original accepted marker"),
            durable,
        );
        assert_eq!(reopened.record_count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_retry_after_acknowledgement_loss_is_idempotent() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x62, 0xa8);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        let terminal_append = journal
            .append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied)
            .expect("terminal disposition");
        assert!(terminal_append.is_newly_committed());
        let terminal = terminal_append.receipt();
        drop(journal);

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("reconcile exact terminal record");
        let retry = reopened
            .append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied)
            .expect("idempotent retry");
        assert_reconciled(retry, terminal);
        assert_eq!(reopened.record_count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn durable_prefix_is_exact_bounded_and_idempotent_across_reopen() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity_with_input_bytes(7, 0x71, 0xb7, 6);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");

        let before_invalid_count = journal.record_count();
        for invalid in [0_u32, input.input_bytes(), input.input_bytes() + 1] {
            assert!(matches!(
                journal.append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::DurablePrefix {
                        applied_bytes: invalid,
                    },
                ),
                Err(GuardianInputJournalError::InvalidAppliedByteCount)
            ));
            assert_eq!(journal.record_count(), before_invalid_count);
        }

        let disposition = GuardianInputDisposition::DurablePrefix { applied_bytes: 3 };
        let prefix_append = journal
            .append_disposition_and_sync(input, disposition)
            .expect("persist exact durable prefix");
        assert!(prefix_append.is_newly_committed());
        let prefix_receipt = prefix_append.receipt();
        let prefix_permit = prefix_append
            .into_terminal_protocol_permit()
            .expect("durable prefix yields exact protocol permit");
        assert_eq!(prefix_permit.identity(), input);
        assert_eq!(prefix_permit.receipt(), prefix_receipt);
        assert_eq!(
            prefix_permit.protocol_state(),
            InputEffectState::DurablePrefix { applied_bytes: 3 }
        );
        assert_eq!(prefix_receipt.disposition(), disposition);
        assert_eq!(disposition.applied_prefix_bytes(), Some(3));
        let terminal_record_count = journal.record_count();
        assert_reconciled(
            journal
                .append_disposition_and_sync(input, disposition)
                .expect("retry exact terminal prefix"),
            prefix_receipt,
        );
        assert_eq!(journal.record_count(), terminal_record_count);
        assert!(matches!(
            journal.append_disposition_and_sync(
                input,
                GuardianInputDisposition::DurablePrefix { applied_bytes: 2 },
            ),
            Err(GuardianInputJournalError::InvalidTransition { .. })
        ));
        assert!(matches!(
            journal.append_disposition_and_sync(input, GuardianInputDisposition::DurableFull),
            Err(GuardianInputJournalError::InvalidTransition { .. })
        ));
        drop(journal);

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover exact durable prefix");
        let recovered = reopened.effect(input.effect_id()).expect("effect recovered");
        assert_eq!(recovered.identity(), input);
        assert_eq!(recovered.input_bytes(), 6);
        assert_eq!(recovered.disposition(), disposition);
        assert_eq!(
            recovered.disposition().recovery_protocol_state(),
            InputEffectState::DurablePrefix { applied_bytes: 3 }
        );
        assert_eq!(recovered.receipt(), prefix_receipt);
        assert_reconciled(
            reopened
                .append_intent_and_sync(input)
                .expect("reconcile prefix through repeated begin"),
            prefix_receipt,
        );
        assert_reconciled(
            reopened
                .append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                )
                .expect("reconcile prefix through repeated acceptance"),
            prefix_receipt,
        );
        assert_reconciled(
            reopened
                .append_disposition_and_sync(input, disposition)
                .expect("reconcile prefix publication acknowledgement loss"),
            prefix_receipt,
        );
        assert_eq!(reopened.record_count(), terminal_record_count);
    }

    #[cfg(unix)]
    #[test]
    fn phase_recovery_maps_only_fully_synchronized_records() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x6c, 0xb1);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        drop(journal);

        let mut journal = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover intent-only phase");
        assert_eq!(
            journal
                .effect(input.effect_id())
                .expect("intent recovered")
                .disposition()
                .recovery_protocol_state(),
            InputEffectState::KnownNotApplied
        );
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        drop(journal);

        let mut journal = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover accepted phase");
        assert_eq!(
            journal
                .effect(input.effect_id())
                .expect("accepted effect recovered")
                .disposition()
                .recovery_protocol_state(),
            InputEffectState::AcceptedNotDurable
        );
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied)
            .expect("definite zero-byte resolution");
        drop(journal);

        let journal = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover terminal resolution");
        assert_eq!(
            journal
                .effect(input.effect_id())
                .expect("terminal effect recovered")
                .disposition()
                .recovery_protocol_state(),
            InputEffectState::KnownNotApplied
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_partial_record_framing_cut_preserves_only_the_committed_prefix() {
        let complete = tempfile::tempdir().expect("create complete fixture tempdir");
        let complete_path = complete.path().join("input.journal");
        let input = identity(7, 0x6d, 0xb2);
        let mut journal = GuardianInputJournal::open(
            create_file(&complete_path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize complete fixture");
        journal
            .sync_parent_directory_and_activate(&open_directory(complete.path()))
            .expect("activate complete fixture");
        journal.append_intent_and_sync(input).expect("intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        drop(journal);
        let physical = std::fs::read(&complete_path).expect("read complete fixture");
        let committed_prefix = FILE_HEADER_BYTES + RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES;
        let second_record_end = committed_prefix + RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES;
        assert_eq!(physical.len(), second_record_end);

        let cuts_dir = tempfile::tempdir().expect("create crash-cut tempdir");
        for trailing_bytes in 1..(RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES) {
            let path = cuts_dir.path().join(format!("cut-{trailing_bytes}.journal"));
            let cut = committed_prefix + trailing_bytes;
            let mut file = create_file(&path);
            file.write_all(&physical[..cut]).expect("write crash cut");
            file.sync_all().expect("sync crash cut fixture");
            drop(file);

            let mut recovered = GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            )
            .expect("recover committed prefix before partial marker");
            assert_eq!(
                recovered.tail(),
                GuardianInputJournalTail::Incomplete {
                    committed_bytes: u64::try_from(committed_prefix)
                        .expect("fixture prefix fits u64"),
                    trailing_bytes: u64::try_from(trailing_bytes)
                        .expect("fixture tail fits u64"),
                }
            );
            let effect = recovered.effect(input.effect_id()).expect("intent retained");
            assert_eq!(effect.disposition(), GuardianInputDisposition::Intent);
            assert_eq!(
                effect.disposition().recovery_protocol_state(),
                InputEffectState::KnownNotApplied
            );
            assert!(matches!(
                recovered.append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                ),
                Err(GuardianInputJournalError::IncompleteTail)
            ));
            assert_eq!(
                std::fs::metadata(&path).expect("crash-cut metadata").len(),
                u64::try_from(cut).expect("fixture cut fits u64")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn partial_terminal_record_cuts_preserve_ambiguous_acceptance_not_a_prefix() {
        let complete = tempfile::tempdir().expect("create complete fixture tempdir");
        let complete_path = complete.path().join("input.journal");
        let input = identity_with_input_bytes(7, 0x72, 0xb8, 6);
        let mut journal = GuardianInputJournal::open(
            create_file(&complete_path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize complete fixture");
        journal
            .sync_parent_directory_and_activate(&open_directory(complete.path()))
            .expect("activate complete fixture");
        journal.append_intent_and_sync(input).expect("intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        journal
            .append_disposition_and_sync(
                input,
                GuardianInputDisposition::DurablePrefix { applied_bytes: 2 },
            )
            .expect("durable prefix");
        drop(journal);

        let physical = std::fs::read(&complete_path).expect("read complete fixture");
        let accepted_prefix = FILE_HEADER_BYTES + 2 * (RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES);
        let cuts = tempfile::tempdir().expect("create terminal crash-cut tempdir");
        for trailing_bytes in 1..(RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES) {
            let path = cuts.path().join(format!("terminal-cut-{trailing_bytes}.journal"));
            let cut = accepted_prefix + trailing_bytes;
            let mut file = create_file(&path);
            file.write_all(&physical[..cut]).expect("write terminal crash cut");
            file.sync_all().expect("sync terminal crash cut");
            drop(file);

            let mut recovered = GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            )
            .expect("recover accepted prefix before incomplete terminal record");
            assert!(matches!(
                recovered.tail(),
                GuardianInputJournalTail::Incomplete {
                    committed_bytes,
                    trailing_bytes: observed,
                } if committed_bytes == u64::try_from(accepted_prefix).unwrap()
                    && observed == u64::try_from(trailing_bytes).unwrap()
            ));
            let effect = recovered.effect(input.effect_id()).expect("accepted effect retained");
            assert_eq!(effect.disposition(), GuardianInputDisposition::AcceptedNotDurable);
            assert_eq!(
                effect.disposition().recovery_protocol_state(),
                InputEffectState::AcceptedNotDurable
            );
            assert!(matches!(
                recovered.append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::DurablePrefix { applied_bytes: 2 },
                ),
                Err(GuardianInputJournalError::IncompleteTail)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn injected_append_failures_poison_and_reconcile_without_replay() {
        for fault in [
            InjectedAppendFault::BeforeWrite,
            InjectedAppendFault::AfterHeader,
            InjectedAppendFault::AfterCiphertext,
            InjectedAppendFault::BeforeSync,
            InjectedAppendFault::AfterSync,
        ] {
            let temp = tempfile::tempdir().expect("create fault fixture tempdir");
            let path = temp.path().join(format!("input-{fault:?}.journal"));
            let input = identity(7, 0x6e, 0xb3);
            let mut journal = GuardianInputJournal::open(
                create_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            )
            .expect("initialize fault fixture");
            journal
                .sync_parent_directory_and_activate(&open_directory(temp.path()))
                .expect("activate fault fixture");
            journal.append_intent_and_sync(input).expect("intent");
            let committed_intent_bytes = journal.committed_bytes();
            journal.inject_append_fault(fault);
            assert!(matches!(
                journal.append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                ),
                Err(GuardianInputJournalError::Io(_))
            ));
            assert!(journal.is_poisoned());
            assert!(matches!(
                journal.append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                ),
                Err(GuardianInputJournalError::Poisoned)
            ));
            drop(journal);

            let mut recovered = GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            )
            .expect("reconcile injected append failure");
            let recovered_disposition = recovered
                .effect(input.effect_id())
                .expect("effect retained")
                .disposition();
            match fault {
                InjectedAppendFault::BeforeWrite => {
                    assert_eq!(recovered.tail(), GuardianInputJournalTail::Clean);
                    assert_eq!(
                        recovered_disposition,
                        GuardianInputDisposition::Intent
                    );
                    assert_eq!(recovered.committed_bytes(), committed_intent_bytes);
                }
                InjectedAppendFault::AfterHeader => {
                    assert!(matches!(
                        recovered.tail(),
                        GuardianInputJournalTail::Incomplete {
                            committed_bytes,
                            trailing_bytes,
                        } if committed_bytes == committed_intent_bytes
                            && trailing_bytes == RECORD_HEADER_BYTES_U64
                    ));
                    assert_eq!(
                        recovered_disposition,
                        GuardianInputDisposition::Intent
                    );
                }
                InjectedAppendFault::AfterCiphertext
                | InjectedAppendFault::BeforeSync
                | InjectedAppendFault::AfterSync => {
                    assert_eq!(recovered.tail(), GuardianInputJournalTail::Clean);
                    assert_eq!(
                        recovered_disposition,
                        GuardianInputDisposition::AcceptedNotDurable
                    );
                    let before_retry = recovered.record_count();
                    let receipt = recovered
                        .append_disposition_and_sync(
                            input,
                            GuardianInputDisposition::AcceptedNotDurable,
                        )
                        .expect("reconcile exact accepted publication");
                    assert_eq!(
                        receipt.disposition(),
                        GuardianInputDisposition::AcceptedNotDurable
                    );
                    assert!(!receipt.is_newly_committed());
                    assert!(receipt.into_first_pty_write_permit().is_none());
                    assert_eq!(recovered.record_count(), before_retry);
                }
            }
            assert_ne!(
                recovered_disposition.recovery_protocol_state(),
                InputEffectState::NotSeen
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepted_effect_may_be_proven_not_applied_but_durable_is_irreversible() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x63, 0xa9);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied)
            .expect("definitive zero-byte result may resolve conservative marker");

        let durable_input = identity(8, 0x6a, 0xaf);
        journal
            .append_intent_and_sync(durable_input)
            .expect("second intent");
        journal
            .append_disposition_and_sync(
                durable_input,
                GuardianInputDisposition::AcceptedNotDurable,
            )
            .expect("second accepted marker");
        journal
            .append_disposition_and_sync(durable_input, GuardianInputDisposition::DurableFull)
            .expect("durable result");
        assert!(matches!(
            journal.append_disposition_and_sync(
                durable_input,
                GuardianInputDisposition::KnownNotApplied,
            ),
            Err(GuardianInputJournalError::InvalidTransition { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn torn_tail_preserves_verified_prefix_and_disables_append() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x64, 0xaa);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        drop(journal);
        let committed = std::fs::metadata(&path).expect("metadata").len();
        let mut physical = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append torn tail");
        physical.write_all(&RECORD_MAGIC[..4]).expect("write torn tail");
        physical.sync_all().expect("sync torn tail");
        drop(physical);

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover committed prefix");
        assert_eq!(
            reopened.tail(),
            GuardianInputJournalTail::Incomplete {
                committed_bytes: committed,
                trailing_bytes: 4,
            }
        );
        assert!(matches!(
            reopened.append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied),
            Err(GuardianInputJournalError::IncompleteTail)
        ));
        assert_eq!(std::fs::metadata(&path).expect("metadata").len(), committed + 4);
    }

    #[cfg(unix)]
    #[test]
    fn wrong_key_and_complete_corruption_fail_closed() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x65, 0xab);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        drop(journal);
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                GuardianOutputCipher::try_from_key_slice(&[0x74; 32]).expect("wrong key valid"),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::KeyIdentityMismatch)
        ));

        let mut file = open_file(&path);
        file.seek(SeekFrom::Start(FILE_HEADER_BYTES_U64 + RECORD_HEADER_BYTES_U64))
            .expect("seek ciphertext");
        file.write_all(&[0xff]).expect("corrupt ciphertext");
        file.sync_all().expect("sync corruption");
        drop(file);
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::RecordDigestMismatch { sequence: 1 })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn torn_file_header_is_preserved_and_never_reinitialized() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let complete_header = encode_file_header(pane(), cipher().key_id());
        let mut file = create_file(&path);
        file.write_all(&complete_header[..FILE_HEADER_BYTES - 1])
            .expect("write torn header");
        file.sync_all().expect("sync torn header");
        drop(file);
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual,
            }) if actual == u64::try_from(FILE_HEADER_BYTES - 1)
                .expect("fixture header length fits u64")
        ));
        assert_eq!(
            std::fs::metadata(&path).expect("torn header metadata").len(),
            u64::try_from(FILE_HEADER_BYTES - 1).expect("fixture header length fits u64")
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_v1_journal_is_preserved_and_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let mut legacy_header = encode_file_header(pane(), cipher().key_id());
        legacy_header[..8].copy_from_slice(&LEGACY_FILE_MAGIC_V1);
        legacy_header[8..12].copy_from_slice(&1_u32.to_le_bytes());
        let mut file = create_file(&path);
        file.write_all(&legacy_header).expect("write legacy header");
        file.sync_all().expect("sync legacy header");
        drop(file);

        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::LegacyV1DispositionAmbiguous)
        ));
        let preserved = std::fs::read(&path).expect("read preserved legacy header");
        assert_eq!(preserved.as_slice(), legacy_header.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn recomputed_outer_digest_cannot_bypass_aead_authentication() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x6b, 0xb0);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        journal.append_intent_and_sync(input).expect("intent");
        drop(journal);

        let mut physical = std::fs::read(&path).expect("read journal bytes");
        let record_offset = FILE_HEADER_BYTES;
        let ciphertext_offset = record_offset + RECORD_HEADER_BYTES;
        physical[ciphertext_offset] ^= 0x80;
        let mut prefix = [0_u8; 32];
        prefix.copy_from_slice(&physical[record_offset..record_offset + 32]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&physical[record_offset + 32..record_offset + 56]);
        let digest = record_digest(
            pane(),
            &prefix,
            &nonce,
            &physical[ciphertext_offset..ciphertext_offset + RECORD_CIPHERTEXT_BYTES],
        );
        physical[record_offset + 56..record_offset + 88].copy_from_slice(&digest);
        let mut file = open_file(&path);
        file.seek(SeekFrom::Start(0)).expect("seek journal start");
        file.write_all(&physical).expect("rewrite tampered journal");
        file.sync_all().expect("sync tampered journal");
        drop(file);

        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::RecordAuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn same_length_terminal_record_splice_cannot_change_the_applied_prefix() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let first = identity_with_input_bytes(7, 0x73, 0xb9, 6);
        let second = identity_with_input_bytes(8, 0x74, 0xba, 6);
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        for (input, applied_bytes) in [(first, 2_u32), (second, 3_u32)] {
            journal.append_intent_and_sync(input).expect("intent");
            journal
                .append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::AcceptedNotDurable,
                )
                .expect("accepted marker");
            journal
                .append_disposition_and_sync(
                    input,
                    GuardianInputDisposition::DurablePrefix { applied_bytes },
                )
                .expect("terminal prefix");
        }
        drop(journal);

        let mut physical = std::fs::read(&path).expect("read journal bytes");
        let record_bytes = RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES;
        let destination = FILE_HEADER_BYTES + 2 * record_bytes;
        let source = FILE_HEADER_BYTES + 5 * record_bytes;
        assert_eq!(physical[destination + 12], physical[source + 12]);

        let mut source_nonce = [0_u8; NONCE_BYTES];
        source_nonce.copy_from_slice(&physical[source + 32..source + 56]);
        let mut source_ciphertext = [0_u8; RECORD_CIPHERTEXT_BYTES];
        source_ciphertext.copy_from_slice(
            &physical[source + RECORD_HEADER_BYTES..source + record_bytes],
        );
        physical[destination + 32..destination + 56].copy_from_slice(&source_nonce);
        physical[destination + RECORD_HEADER_BYTES..destination + record_bytes]
            .copy_from_slice(&source_ciphertext);

        let mut destination_prefix = [0_u8; 32];
        destination_prefix.copy_from_slice(&physical[destination..destination + 32]);
        let digest = record_digest(
            pane(),
            &destination_prefix,
            &source_nonce,
            &source_ciphertext,
        );
        physical[destination + 56..destination + 88].copy_from_slice(&digest);
        let mut file = open_file(&path);
        file.seek(SeekFrom::Start(0)).expect("seek journal start");
        file.write_all(&physical).expect("write spliced journal");
        file.sync_all().expect("sync spliced journal");
        drop(file);

        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::RecordAuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_length_change_poison_is_sticky() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        let mut external = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open external writer");
        external.write_all(&[0]).expect("change length");
        external.sync_all().expect("sync external byte");
        assert!(matches!(
            journal.append_intent_and_sync(identity(7, 0x66, 0xac)),
            Err(GuardianInputJournalError::ExternalLengthChange { .. })
        ));
        assert!(journal.is_poisoned());
        assert!(matches!(
            journal.append_intent_and_sync(identity(8, 0x67, 0xad)),
            Err(GuardianInputJournalError::Poisoned)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn activation_identity_order_and_capacity_fail_closed() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let limits = GuardianInputJournalLimits {
            max_log_bytes: FILE_HEADER_BYTES_U64 + 2 * RECORD_BYTES_U64,
            max_records: 2,
            max_effects: 2,
            max_input_bytes: 4,
        };
        let mut journal = GuardianInputJournal::open(
            create_file(&path),
            pane(),
            cipher(),
            limits,
        )
        .expect("initialize bounded journal");
        let first = identity(7, 0x68, 0xae);
        assert!(matches!(
            journal.append_intent_and_sync(first),
            Err(GuardianInputJournalError::DirectoryEntryNotDurable)
        ));
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate journal");
        assert!(matches!(
            journal.append_intent_and_sync(identity_with_input_bytes(7, 0x68, 0xae, 5)),
            Err(GuardianInputJournalError::InvalidInputLength)
        ));
        journal.append_intent_and_sync(first).expect("first intent");
        assert!(matches!(
            journal.append_intent_and_sync(identity(6, 0x69, 0xaf)),
            Err(GuardianInputJournalError::NonMonotonicInputIdentity)
        ));
        journal
            .append_intent_and_sync(identity(8, 0x69, 0xaf))
            .expect("second monotonic intent consumes final record");
        assert!(matches!(
            journal.append_disposition_and_sync(
                first,
                GuardianInputDisposition::AcceptedNotDurable,
            ),
            Err(GuardianInputJournalError::RecordLimit { maximum: 2 })
        ));
    }
}
