//! Durable input-effect intent and disposition journal for the guardian.
//!
//! Capacity for an input's complete Intent + `AcceptedNotDurable` + terminal
//! lifecycle is reserved before Intent can be synchronized. Intent and then
//! `AcceptedNotDurable` are synchronized before any bytes may become observable
//! to a child PTY. The caller then refines that
//! conservative marker to `DurableFull`, to an exact `DurablePrefix`, or to
//! `KnownNotApplied` only when it can prove that zero bytes became observable.
//! A crash after the accepted marker is never interpreted as permission to
//! replay input; recovery retains the ambiguous effect and takeover stays
//! fenced. A durable prefix is terminal for the exact request: retries return
//! its original receipt and must never apply the known prefix again.
//!
//! Raw input is never persisted. Even its payload digest is encrypted because
//! hashes of small key events are enumerable. The fixed-size encrypted records
//! use the guardian journal key with an input-specific AEAD domain. The v3 file
//! header and every record's AEAD associated data bind the exact guardian
//! incarnation as well as the durable pane, preventing a prior incarnation's
//! file or record from authenticating as current authority. This module accepts
//! only caller-owned file descriptors; secure path traversal and key
//! provisioning remain service-layer responsibilities.

use crate::guardian_output_journal::{GuardianOutputCipher, GuardianOutputJournalError};
use crate::guardian_protocol::{
    AuthenticatedGuardianRequest, GuardianEffectTransactionError, GuardianInputEffectIdentity,
    GuardianProtocolError, GuardianProtocolState, GuardianReply, InputEffectState,
    GUARDIAN_MAX_INPUT_BYTES,
};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use thiserror::Error;
use uuid::Uuid;

const FILE_MAGIC: [u8; 8] = *b"FTGINP03";
const RECORD_MAGIC: [u8; 8] = *b"FTGIR003";
const LEGACY_FILE_MAGIC_V1: [u8; 8] = *b"FTGINP01";
const LEGACY_FILE_MAGIC_V2: [u8; 8] = *b"FTGINP02";
const FORMAT_VERSION: u32 = 3;
const FILE_HEADER_BYTES: usize = 128;
const FILE_HEADER_BYTES_U32: u32 = 128;
const FILE_HEADER_BYTES_U64: u64 = 128;
const RECORD_HEADER_BYTES: usize = 96;
const RECORD_HEADER_BYTES_U32: u32 = 96;
#[cfg(test)]
const RECORD_HEADER_BYTES_U64: u64 = 96;
const RECORD_PLAINTEXT_BYTES: usize = 136;
const RECORD_PLAINTEXT_BYTES_U32: u32 = 136;
const RECORD_CIPHERTEXT_BYTES: usize = 152;
const RECORD_CIPHERTEXT_BYTES_U32: u32 = 152;
const RECORD_BYTES_U64: u64 = 248;
const NONCE_BYTES: usize = 24;
const KEY_ID_BYTES: usize = 8;
const FILE_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-input-file.v3\0";
const RECORD_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-input-record.v3\0";
const RECORD_AAD_DOMAIN: &[u8] = b"frankenterm.guardian-input-aead.v3\0";
const RECORDS_PER_COMPLETE_EFFECT: u64 = 3;

/// Content-free marker returned when the audited live-input worker boundary
/// recovered a panic from journal, protocol, or PTY-writer code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianInputWorkerPanic;

/// Keep the worker's sole protocol/writer/journal authority outside the panic
/// boundary while isolating one input transaction.
///
/// The service passes a closure that borrows its owned job bundle.  If any
/// inner component panics, unwinding stops here and the caller still owns the
/// bundle, so it can restore the protocol authority and retain the durable
/// `AcceptedNotDurable` fence without ever minting a second write permit.
pub fn catch_guardian_input_worker_panic<R>(
    operation: impl FnOnce() -> R,
) -> Result<R, GuardianInputWorkerPanic> {
    catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(operation),
    )
    .map_err(|_| GuardianInputWorkerPanic)
}

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
            Self::Intent | Self::AcceptedNotDurable | Self::DurableFull | Self::KnownNotApplied => {
                None
            }
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
            Self::Intent | Self::AcceptedNotDurable | Self::DurableFull | Self::KnownNotApplied => {
                0
            }
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

    fn validate_for_input_bytes(self, input_bytes: u32) -> Result<(), GuardianInputJournalError> {
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
    /// A scanned `Intent` is `DispositionUnavailable`, not replay authority.
    /// Although the append ordering forbids the PTY call before a later
    /// `AcceptedNotDurable` record, a valid-prefix scan alone cannot prove that
    /// a formerly present terminal suffix was not rolled back. An accepted
    /// marker remains pending and must never be replayed after a crash.
    #[must_use]
    pub const fn recovery_protocol_state(self) -> InputEffectState {
        match self {
            Self::Intent => InputEffectState::DispositionUnavailable,
            Self::KnownNotApplied => InputEffectState::KnownNotApplied,
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
pub(crate) struct GuardianInputJournalAppend {
    identity: GuardianInputEffectIdentity,
    receipt: GuardianInputJournalReceipt,
    newly_committed: bool,
}

/// Opaque one-shot journal authority for the first PTY write attempt.
///
/// The live PTY adapter consumes this value together with the newly admitted
/// protocol transaction. It is intentionally neither `Clone` nor `Copy`, and
/// external callers cannot construct it.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the guardian input write permit must be consumed by the one PTY write attempt"]
pub struct GuardianInputWritePermit {
    identity: GuardianInputEffectIdentity,
}

/// Content-free result of consuming one exact PTY-write permit.
///
/// `disposition == None` means the writer violated the `Write` contract by
/// reporting more bytes than it was given. The caller must retain the durable
/// `AcceptedNotDurable` fence in that case; it has no defensible exact prefix
/// to publish. The payload itself is never retained here.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the guardian input write outcome must be durably reconciled"]
pub struct GuardianInputWriteOutcome {
    identity: GuardianInputEffectIdentity,
    disposition: Option<GuardianInputDisposition>,
    writer_invoked: bool,
}

/// Opaque journal-to-protocol authority for one exact terminal disposition.
///
/// The applied prefix is carried inside the private receipt and therefore
/// cannot diverge between durable publication and protocol completion.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the terminal input permit must reconcile the exact durable disposition"]
pub(crate) struct GuardianInputTerminalPermit {
    identity: GuardianInputEffectIdentity,
    receipt: GuardianInputJournalReceipt,
}

/// Opaque, journal-backed authority to reconcile one terminal input outcome
/// into protocol state after the service revalidates its filesystem authority.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the committed guardian input outcome must reconcile protocol state"]
pub struct GuardianInputProtocolCompletion {
    terminal: GuardianInputTerminalPermit,
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

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn receipt(&self) -> GuardianInputJournalReceipt {
        self.receipt
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_newly_committed(&self) -> bool {
        self.newly_committed
    }

    #[must_use]
    pub(crate) const fn disposition(&self) -> GuardianInputDisposition {
        self.receipt.disposition()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    /// Consume the append result and return its one-shot PTY-write permit.
    ///
    /// The caller must also be executing the newly admitted protocol effect;
    /// a reconciled append can never authorize a PTY write.
    #[must_use]
    pub(crate) fn into_first_pty_write_permit(self) -> Option<GuardianInputWritePermit> {
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
    pub(crate) fn into_terminal_protocol_permit(self) -> Option<GuardianInputTerminalPermit> {
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

    /// Consume this permit in exactly one PTY `write` call.
    ///
    /// The authenticated length and digest are checked before the writer is
    /// invoked. A payload mismatch therefore resolves to `KnownNotApplied`.
    /// `Write::write` promises that an `Err` writes no bytes, so errors and
    /// zero-byte successes have the same exact disposition. Interrupted writes
    /// are deliberately not retried: doing so would turn one journal permit
    /// into multiple externally observable write attempts.
    pub fn write_once<W: Write + ?Sized>(
        self,
        writer: &mut W,
        payload: &[u8],
    ) -> GuardianInputWriteOutcome {
        let identity = self.identity;
        let payload_bytes = u32::try_from(payload.len()).ok();
        let payload_sha256: [u8; 32] = Sha256::digest(payload).into();
        if payload_bytes != Some(identity.input_bytes())
            || payload_sha256 != identity.payload_sha256()
        {
            return GuardianInputWriteOutcome {
                identity,
                disposition: Some(GuardianInputDisposition::KnownNotApplied),
                writer_invoked: false,
            };
        }

        let disposition = match writer.write(payload) {
            Ok(0) | Err(_) => Some(GuardianInputDisposition::KnownNotApplied),
            Ok(applied_bytes) if applied_bytes == payload.len() => {
                Some(GuardianInputDisposition::DurableFull)
            }
            Ok(applied_bytes) if applied_bytes < payload.len() => u32::try_from(applied_bytes)
                .ok()
                .map(|applied_bytes| GuardianInputDisposition::DurablePrefix { applied_bytes }),
            Ok(_) => None,
        };
        GuardianInputWriteOutcome {
            identity,
            disposition,
            writer_invoked: true,
        }
    }
}

impl GuardianInputWriteOutcome {
    #[must_use]
    pub const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    /// Exact terminal disposition, or `None` when the writer's byte count made
    /// the observable prefix indeterminate.
    #[must_use]
    pub const fn disposition(&self) -> Option<GuardianInputDisposition> {
        self.disposition
    }

    #[must_use]
    pub const fn writer_was_invoked(&self) -> bool {
        self.writer_invoked
    }

    #[must_use]
    pub const fn applied_bytes(&self) -> Option<u32> {
        match self.disposition {
            Some(GuardianInputDisposition::DurableFull) => Some(self.identity.input_bytes()),
            Some(GuardianInputDisposition::DurablePrefix { applied_bytes }) => Some(applied_bytes),
            Some(GuardianInputDisposition::KnownNotApplied) => Some(0),
            Some(
                GuardianInputDisposition::Intent | GuardianInputDisposition::AcceptedNotDurable,
            )
            | None => None,
        }
    }
}

/// Result of joining protocol admission to the exact durable journal phase.
#[derive(Debug)]
pub enum GuardianInputTransaction {
    Reconciled(GuardianReply),
    WriteAuthorized {
        accepted_reply: GuardianReply,
        permit: GuardianInputWritePermit,
    },
}

pub enum GuardianInputTransactionError {
    Protocol(GuardianProtocolError),
    JournalBeforeWrite(GuardianInputJournalError),
    OutcomeIndeterminate(GuardianReply),
    AcceptedJournalUnavailable {
        accepted_reply: GuardianReply,
        error: GuardianInputJournalError,
    },
    AcceptedProtocolUnavailable {
        accepted_reply: GuardianReply,
        error: GuardianProtocolError,
    },
}

impl std::fmt::Debug for GuardianInputTransactionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(error) => formatter.debug_tuple("Protocol").field(error).finish(),
            Self::JournalBeforeWrite(_) => formatter.write_str("JournalBeforeWrite"),
            Self::OutcomeIndeterminate(_) => formatter.write_str("OutcomeIndeterminate"),
            Self::AcceptedJournalUnavailable { .. } => {
                formatter.write_str("AcceptedJournalUnavailable")
            }
            Self::AcceptedProtocolUnavailable { error, .. } => formatter
                .debug_tuple("AcceptedProtocolUnavailable")
                .field(error)
                .finish(),
        }
    }
}

/// Failure while durably publishing the exact result of the one authorized PTY
/// write. This seam never grants write authority; it only converts an outcome
/// minted by [`GuardianInputWritePermit::write_once`] into an opaque protocol
/// completion after synchronizing its terminal WAL record.
#[derive(Debug, Error)]
pub enum GuardianInputCompletionError {
    #[error("guardian input write outcome has no exact terminal disposition")]
    DispositionIndeterminate,
    #[error("guardian input terminal journal publication failed")]
    Journal(#[source] GuardianInputJournalError),
    #[error("guardian input terminal journal record did not yield protocol authority")]
    StateInvariant,
}

/// Join protocol admission to the per-pane WAL without ever authorizing an
/// exact retry to write again.
///
/// The protocol callback runs only for a new effect identity. It synchronizes
/// `Intent` and then `AcceptedNotDurable`; only a newly committed accepted
/// marker can produce the non-cloneable write permit. When protocol admission
/// is an exact replay, the current WAL phase is inspected solely for terminal
/// reconciliation and cannot yield fresh write authority. A reopened journal,
/// including a valid header-only prefix, may serve exact idempotent receipts
/// but cannot append or advance a phase until a separate durable anti-rollback
/// high-water proof exists.
pub fn begin_guardian_input_transaction(
    protocol: &mut GuardianProtocolState,
    journal: &mut GuardianInputJournal,
    request: &AuthenticatedGuardianRequest,
) -> Result<GuardianInputTransaction, GuardianInputTransactionError> {
    let identity = GuardianInputEffectIdentity::from_authenticated_request(request)
        .map_err(GuardianInputTransactionError::Protocol)?;
    let mut prepared_append = None;
    let admitted = protocol.apply_input_effect_transactionally(request, |_| {
        let intent = journal.append_intent_and_sync(identity)?;
        let append = if intent.disposition() == GuardianInputDisposition::Intent {
            journal.append_disposition_and_sync(
                identity,
                GuardianInputDisposition::AcceptedNotDurable,
            )?
        } else {
            intent
        };
        prepared_append = Some(append);
        Ok(())
    });
    let reply = match admitted {
        Ok(reply) => reply,
        Err(GuardianEffectTransactionError::Protocol(error)) => {
            return Err(GuardianInputTransactionError::Protocol(error));
        }
        Err(GuardianEffectTransactionError::Effect(error)) => {
            return Err(GuardianInputTransactionError::JournalBeforeWrite(error));
        }
        Err(GuardianEffectTransactionError::OutcomeIndeterminate(reply)) => {
            return Err(GuardianInputTransactionError::OutcomeIndeterminate(reply));
        }
    };

    let append = match prepared_append {
        Some(append) => append,
        None => {
            let existing = journal.effect(identity.effect_id()).ok_or_else(|| {
                GuardianInputTransactionError::AcceptedJournalUnavailable {
                    accepted_reply: reply.clone(),
                    error: GuardianInputJournalError::EffectIdentityConflict,
                }
            })?;
            if existing.identity() != identity {
                return Err(GuardianInputTransactionError::AcceptedJournalUnavailable {
                    accepted_reply: reply,
                    error: GuardianInputJournalError::EffectIdentityConflict,
                });
            }
            GuardianInputJournalAppend::reconciled(existing.identity(), existing.receipt())
        }
    };
    match append.disposition() {
        GuardianInputDisposition::DurableFull
        | GuardianInputDisposition::DurablePrefix { .. }
        | GuardianInputDisposition::KnownNotApplied => {
            let permit = append.into_terminal_protocol_permit().ok_or_else(|| {
                GuardianInputTransactionError::AcceptedProtocolUnavailable {
                    accepted_reply: reply.clone(),
                    error: GuardianProtocolError::StateInvariantViolation(
                        "guardian-input-terminal-permit",
                    ),
                }
            })?;
            permit
                .reconcile_protocol(protocol)
                .map(GuardianInputTransaction::Reconciled)
                .map_err(
                    |error| GuardianInputTransactionError::AcceptedProtocolUnavailable {
                        accepted_reply: reply,
                        error,
                    },
                )
        }
        GuardianInputDisposition::AcceptedNotDurable => {
            match append.into_first_pty_write_permit() {
                Some(permit) => Ok(GuardianInputTransaction::WriteAuthorized {
                    accepted_reply: reply,
                    permit,
                }),
                None => Ok(GuardianInputTransaction::Reconciled(reply)),
            }
        }
        GuardianInputDisposition::Intent => Ok(GuardianInputTransaction::Reconciled(reply)),
    }
}

/// Synchronize the terminal result of the one authorized PTY write and return
/// opaque protocol-completion authority.
///
/// Keeping the raw append and permit conversions crate-private prevents an
/// external caller from manufacturing a write permit by publishing an
/// `AcceptedNotDurable` record directly. The service may revalidate its pinned
/// path after this call and before consuming the returned completion.
pub fn commit_guardian_input_outcome(
    journal: &mut GuardianInputJournal,
    outcome: GuardianInputWriteOutcome,
) -> Result<GuardianInputProtocolCompletion, GuardianInputCompletionError> {
    let disposition = outcome
        .disposition()
        .ok_or(GuardianInputCompletionError::DispositionIndeterminate)?;
    if !disposition.is_terminal() {
        return Err(GuardianInputCompletionError::DispositionIndeterminate);
    }
    let append = journal
        .append_disposition_and_sync(outcome.identity(), disposition)
        .map_err(GuardianInputCompletionError::Journal)?;
    let terminal = append
        .into_terminal_protocol_permit()
        .ok_or(GuardianInputCompletionError::StateInvariant)?;
    Ok(GuardianInputProtocolCompletion { terminal })
}

/// Return an already retained exact input receipt when the live PTY/journal
/// owner has been released, while proving that no new input can be admitted.
pub fn replay_guardian_input_without_writer(
    protocol: &mut GuardianProtocolState,
    request: &AuthenticatedGuardianRequest,
) -> Result<GuardianReply, GuardianEffectTransactionError<()>> {
    protocol.apply_input_effect_transactionally(request, |_| Err(()))
}

impl GuardianInputTerminalPermit {
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn identity(&self) -> GuardianInputEffectIdentity {
        self.identity
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn receipt(&self) -> GuardianInputJournalReceipt {
        self.receipt
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn protocol_state(&self) -> InputEffectState {
        self.receipt.disposition.recovery_protocol_state()
    }

    /// Consume this exact durable result and complete the protocol once.
    pub(crate) fn reconcile_protocol(
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
            GuardianInputDisposition::Intent | GuardianInputDisposition::AcceptedNotDurable => Err(
                GuardianProtocolError::StateInvariantViolation("input-terminal-permit-disposition"),
            ),
        }
    }
}

impl GuardianInputProtocolCompletion {
    /// Consume the exact journal-backed terminal result and complete protocol
    /// state once. Exact retries remain idempotent inside the protocol.
    pub fn reconcile_protocol(
        self,
        protocol: &mut GuardianProtocolState,
    ) -> Result<GuardianReply, GuardianProtocolError> {
        self.terminal.reconcile_protocol(protocol)
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
    #[error("new guardian input journal descriptor is not empty: found {observed} bytes")]
    NewJournalNotEmpty { observed: u64 },
    #[error("guardian input journal file header is torn: found {actual} of {expected} bytes")]
    TornFileHeader { expected: usize, actual: u64 },
    #[error("guardian input journal file magic is invalid")]
    InvalidFileMagic,
    #[error(
        "guardian input journal v1 has an ambiguous fieldless durable disposition and requires conservative offline migration"
    )]
    LegacyV1DispositionAmbiguous,
    #[error(
        "guardian input journal v2 is not bound to a guardian incarnation and requires conservative offline migration"
    )]
    LegacyV2GuardianIdentityUnbound,
    #[error("unsupported guardian input journal version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("guardian input journal file header length is invalid: {observed}")]
    InvalidFileHeaderLength { observed: u32 },
    #[error("guardian input journal belongs to another durable pane")]
    PaneIdentityMismatch,
    #[error("guardian input journal belongs to another guardian incarnation")]
    GuardianIncarnationMismatch,
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
    #[error(
        "guardian input journal append authority is withheld after reopen until a durable anti-rollback high-water proof exists"
    )]
    RecoveryAuthorityUnavailable,
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
    guardian_incarnation: Uuid,
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
    recovery_append_authority_withheld: bool,
    poisoned: bool,
    #[cfg(test)]
    injected_append_fault: Option<InjectedAppendFault>,
}

impl GuardianInputJournal {
    /// Initialize one descriptor proven to have been created exclusively for
    /// this journal by the service-layer path authority.
    ///
    /// The descriptor must still be empty. This explicit constructor prevents a
    /// truncated recovery file from acquiring append authority merely because
    /// its observed length is zero.
    pub fn create(
        file: File,
        durable_pane_id: Uuid,
        guardian_incarnation: Uuid,
        cipher: GuardianOutputCipher,
        limits: GuardianInputJournalLimits,
    ) -> Result<Self, GuardianInputJournalError> {
        Self::open_inner(
            file,
            durable_pane_id,
            guardian_incarnation,
            cipher,
            limits,
            true,
        )
    }

    /// Scan one existing pane journal under an externally authenticated
    /// guardian incarnation without granting recovery append authority.
    ///
    /// Every recovery scan, including a valid header-only prefix, is
    /// scan/idempotent-receipt authority only; it cannot append until a durable
    /// anti-rollback high-water proof is added.
    pub fn open(
        file: File,
        durable_pane_id: Uuid,
        guardian_incarnation: Uuid,
        cipher: GuardianOutputCipher,
        limits: GuardianInputJournalLimits,
    ) -> Result<Self, GuardianInputJournalError> {
        Self::open_inner(
            file,
            durable_pane_id,
            guardian_incarnation,
            cipher,
            limits,
            false,
        )
    }

    fn open_inner(
        mut file: File,
        durable_pane_id: Uuid,
        guardian_incarnation: Uuid,
        cipher: GuardianOutputCipher,
        limits: GuardianInputJournalLimits,
        initialize_new: bool,
    ) -> Result<Self, GuardianInputJournalError> {
        if durable_pane_id.is_nil() || guardian_incarnation.is_nil() {
            return Err(GuardianInputJournalError::InvalidIdentity);
        }
        let limits = limits.validate()?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianInputJournalError::NotRegularFile);
        }
        let mut physical_bytes = metadata.len();
        if initialize_new && physical_bytes != 0 {
            return Err(GuardianInputJournalError::NewJournalNotEmpty {
                observed: physical_bytes,
            });
        }
        if physical_bytes > limits.max_log_bytes {
            return Err(GuardianInputJournalError::LogByteLimit {
                observed: physical_bytes,
                maximum: limits.max_log_bytes,
            });
        }
        if initialize_new {
            let header = encode_file_header(durable_pane_id, guardian_incarnation, cipher.key_id());
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
            guardian_incarnation,
            &cipher,
            limits,
        )?;
        // A header-only recovery scan is also ambiguous without an external
        // high-water mark: a previously nonempty log can be rolled back to that
        // valid prefix. Only the process holding create-new provenance may
        // append.
        let recovery_append_authority_withheld = !initialize_new;
        Ok(Self {
            file,
            durable_pane_id,
            guardian_incarnation,
            cipher,
            limits,
            committed_bytes: scan.committed_bytes,
            record_count: scan.record_count,
            next_journal_sequence: scan.next_journal_sequence,
            terminal_record_digest: scan.terminal_record_digest,
            tail: scan.tail,
            effects: scan.effects,
            last_input_identity: scan.last_input_identity,
            directory_entry_sync_required: initialize_new,
            recovery_append_authority_withheld,
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
    pub const fn guardian_incarnation(&self) -> Uuid {
        self.guardian_incarnation
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
    fn fail_append_if_injected(&mut self, stage: InjectedAppendFault) -> std::io::Result<()> {
        if self.injected_append_fault == Some(stage) {
            self.injected_append_fault = None;
            Err(std::io::Error::other(
                "injected guardian input append fault",
            ))
        } else {
            Ok(())
        }
    }

    /// Synchronize a new intent before any input bytes are forwarded.
    ///
    /// This first proves capacity for all three records in the effect lifecycle
    /// plus the outstanding follow-ups reserved by every incomplete effect.
    pub(crate) fn append_intent_and_sync(
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
        let reserved_followups = self.reserved_followup_records(None)?;
        let complete_lifecycle_records = reserved_followups
            .checked_add(RECORDS_PER_COMPLETE_EFFECT)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        self.ensure_append_capacity(complete_lifecycle_records)?;
        self.append_record(identity, GuardianInputDisposition::Intent)
            .map(|receipt| GuardianInputJournalAppend::committed(identity, receipt))
    }

    /// Synchronize one exact state transition for an already durable intent.
    pub(crate) fn append_disposition_and_sync(
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
        let reserved_followups = self.reserved_followup_records(Some(identity.effect_id()))?;
        let records_required = reserved_followups
            .checked_add(followup_records(disposition))
            .and_then(|required| required.checked_add(1))
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        self.ensure_append_capacity(records_required)?;
        self.append_record(identity, disposition)
            .map(|receipt| GuardianInputJournalAppend::committed(identity, receipt))
    }

    fn reserved_followup_records(
        &self,
        excluded_effect: Option<Uuid>,
    ) -> Result<u64, GuardianInputJournalError> {
        self.effects
            .iter()
            .filter(|(effect_id, _)| Some(**effect_id) != excluded_effect)
            .try_fold(0_u64, |reserved, (_, effect)| {
                reserved
                    .checked_add(followup_records(effect.disposition))
                    .ok_or(GuardianInputJournalError::ArithmeticOverflow)
            })
    }

    /// Prove that every record already promised to an admitted effect plus the
    /// requested append(s) fits before any new write authority can be issued.
    fn ensure_append_capacity(
        &mut self,
        records_required: u64,
    ) -> Result<(), GuardianInputJournalError> {
        if self.poisoned {
            return Err(GuardianInputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required {
            return Err(GuardianInputJournalError::DirectoryEntryNotDurable);
        }
        if self.tail != GuardianInputJournalTail::Clean {
            return Err(GuardianInputJournalError::IncompleteTail);
        }
        if self.recovery_append_authority_withheld {
            return Err(GuardianInputJournalError::RecoveryAuthorityUnavailable);
        }
        let physical_bytes = self.file.metadata()?.len();
        if physical_bytes != self.committed_bytes {
            self.poisoned = true;
            return Err(GuardianInputJournalError::ExternalLengthChange {
                expected: self.committed_bytes,
                observed: physical_bytes,
            });
        }
        let projected_records = self
            .record_count
            .checked_add(records_required)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        if projected_records > self.limits.max_records {
            return Err(GuardianInputJournalError::RecordLimit {
                maximum: self.limits.max_records,
            });
        }
        let appended_bytes = RECORD_BYTES_U64
            .checked_mul(records_required)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        let projected_bytes = self
            .committed_bytes
            .checked_add(appended_bytes)
            .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
        if projected_bytes > self.limits.max_log_bytes {
            return Err(GuardianInputJournalError::LogByteLimit {
                observed: projected_bytes,
                maximum: self.limits.max_log_bytes,
            });
        }
        if records_required != 0 {
            let final_sequence_delta = records_required
                .checked_sub(1)
                .ok_or(GuardianInputJournalError::ArithmeticOverflow)?;
            self.next_journal_sequence
                .and_then(|sequence| sequence.checked_add(final_sequence_delta))
                .ok_or(GuardianInputJournalError::JournalSequenceExhausted)?;
        }
        Ok(())
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
        self.ensure_append_capacity(1)?;
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
        let input_bytes = identity.input_bytes();
        disposition.validate_for_input_bytes(input_bytes)?;
        let plaintext = encode_plaintext(identity, disposition, self.terminal_record_digest);
        let header_prefix = encode_record_header_prefix(journal_sequence, disposition);
        let aad = record_aad(
            self.durable_pane_id,
            self.guardian_incarnation,
            &header_prefix,
        );
        let (nonce, ciphertext) = self
            .cipher
            .seal_guardian_metadata(&plaintext, &aad)
            .map_err(GuardianInputJournalError::Encryption)?;
        if ciphertext.len() != RECORD_CIPHERTEXT_BYTES {
            return Err(GuardianInputJournalError::ArithmeticOverflow);
        }
        let record_digest = record_digest(
            self.durable_pane_id,
            self.guardian_incarnation,
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

const fn followup_records(disposition: GuardianInputDisposition) -> u64 {
    match disposition {
        GuardianInputDisposition::Intent => 2,
        GuardianInputDisposition::AcceptedNotDurable => 1,
        GuardianInputDisposition::DurableFull
        | GuardianInputDisposition::DurablePrefix { .. }
        | GuardianInputDisposition::KnownNotApplied => 0,
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
            GuardianInputDisposition::DurableFull | GuardianInputDisposition::DurablePrefix { .. }
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

fn encode_file_header(
    durable_pane_id: Uuid,
    guardian_incarnation: Uuid,
    key_id: [u8; KEY_ID_BYTES],
) -> [u8; FILE_HEADER_BYTES] {
    let mut header = [0_u8; FILE_HEADER_BYTES];
    header[0..8].copy_from_slice(&FILE_MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&FILE_HEADER_BYTES_U32.to_le_bytes());
    header[16..32].copy_from_slice(durable_pane_id.as_bytes());
    header[32..40].copy_from_slice(&key_id);
    header[40..56].copy_from_slice(guardian_incarnation.as_bytes());
    let mut hasher = Sha256::new();
    hasher.update(FILE_DIGEST_DOMAIN);
    hasher.update(&header[..96]);
    header[96..128].copy_from_slice(&hasher.finalize());
    header
}

fn validate_file_header(
    header: &[u8; FILE_HEADER_BYTES],
    durable_pane_id: Uuid,
    guardian_incarnation: Uuid,
    key_id: [u8; KEY_ID_BYTES],
) -> Result<(), GuardianInputJournalError> {
    if header[0..8] == LEGACY_FILE_MAGIC_V1 {
        return Err(GuardianInputJournalError::LegacyV1DispositionAmbiguous);
    }
    if header[0..8] == LEGACY_FILE_MAGIC_V2 {
        return Err(GuardianInputJournalError::LegacyV2GuardianIdentityUnbound);
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
    if header[40..56] != *guardian_incarnation.as_bytes() {
        return Err(GuardianInputJournalError::GuardianIncarnationMismatch);
    }
    if header[56..96].iter().any(|byte| *byte != 0) {
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

fn record_aad(durable_pane_id: Uuid, guardian_incarnation: Uuid, prefix: &[u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RECORD_AAD_DOMAIN.len() + 32 + prefix.len());
    aad.extend_from_slice(RECORD_AAD_DOMAIN);
    aad.extend_from_slice(durable_pane_id.as_bytes());
    aad.extend_from_slice(guardian_incarnation.as_bytes());
    aad.extend_from_slice(prefix);
    aad
}

fn record_digest(
    durable_pane_id: Uuid,
    guardian_incarnation: Uuid,
    prefix: &[u8; 32],
    nonce: &[u8; NONCE_BYTES],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(durable_pane_id.as_bytes());
    hasher.update(guardian_incarnation.as_bytes());
    hasher.update(prefix);
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().into()
}

fn scan_journal(
    file: &mut File,
    physical_bytes: u64,
    durable_pane_id: Uuid,
    guardian_incarnation: Uuid,
    cipher: &GuardianOutputCipher,
    limits: GuardianInputJournalLimits,
) -> Result<JournalScan, GuardianInputJournalError> {
    file.seek(SeekFrom::Start(0))?;
    let mut file_header = [0_u8; FILE_HEADER_BYTES];
    file.read_exact(&mut file_header)?;
    validate_file_header(
        &file_header,
        durable_pane_id,
        guardian_incarnation,
        cipher.key_id(),
    )?;

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
        let expected_sequence =
            next_journal_sequence.ok_or(GuardianInputJournalError::JournalSequenceExhausted)?;
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
        let expected_digest = record_digest(
            durable_pane_id,
            guardian_incarnation,
            &prefix,
            &nonce,
            &ciphertext,
        );
        if header[56..88] != expected_digest {
            return Err(GuardianInputJournalError::RecordDigestMismatch {
                sequence: journal_sequence,
            });
        }
        let aad = record_aad(durable_pane_id, guardian_incarnation, &prefix);
        let plaintext = cipher
            .open_guardian_metadata(&nonce, &ciphertext, &aad)
            .map_err(|_| GuardianInputJournalError::RecordAuthenticationFailed)?;
        if plaintext.len() != RECORD_PLAINTEXT_BYTES || plaintext[88..104].iter().any(|b| *b != 0) {
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

    fn pane() -> Uuid {
        Uuid::from_bytes([0x41; 16])
    }

    fn guardian() -> Uuid {
        Uuid::from_bytes([0x31; 16])
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
            InputEffectState::DispositionUnavailable
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
            GuardianInputDisposition::DurablePrefix { applied_bytes: 2 }.recovery_protocol_state(),
            InputEffectState::DurablePrefix { applied_bytes: 2 }
        );
    }

    #[test]
    fn one_shot_write_permit_classifies_exact_write_boundaries_without_retry() {
        #[derive(Clone, Copy)]
        enum WriteMode {
            Full,
            Prefix,
            Zero,
            Error,
            OverReported,
        }

        struct Writer {
            mode: WriteMode,
            calls: u32,
        }

        impl std::io::Write for Writer {
            fn write(&mut self, payload: &[u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.mode {
                    WriteMode::Full => Ok(payload.len()),
                    WriteMode::Prefix => Ok(2),
                    WriteMode::Zero => Ok(0),
                    WriteMode::Error => Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "injected one-shot write interruption",
                    )),
                    WriteMode::OverReported => Ok(payload.len() + 1),
                }
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let payload = b"abcd";
        let exact_identity = GuardianInputEffectIdentity::new(
            pane(),
            Uuid::from_bytes([0x52; 16]),
            3,
            1,
            Uuid::from_bytes([0x91; 16]),
            u32::try_from(payload.len()).unwrap(),
            Sha256::digest(payload).into(),
        )
        .unwrap();
        let permit = || GuardianInputWritePermit {
            identity: exact_identity,
        };

        let mut full = Writer {
            mode: WriteMode::Full,
            calls: 0,
        };
        let outcome = permit().write_once(&mut full, payload);
        assert_eq!(full.calls, 1);
        assert_eq!(
            outcome.disposition(),
            Some(GuardianInputDisposition::DurableFull)
        );
        assert_eq!(outcome.applied_bytes(), Some(4));

        let mut partial = Writer {
            mode: WriteMode::Prefix,
            calls: 0,
        };
        let outcome = permit().write_once(&mut partial, payload);
        assert_eq!(partial.calls, 1);
        assert_eq!(
            outcome.disposition(),
            Some(GuardianInputDisposition::DurablePrefix { applied_bytes: 2 })
        );

        for mode in [WriteMode::Zero, WriteMode::Error] {
            let mut writer = Writer { mode, calls: 0 };
            let outcome = permit().write_once(&mut writer, payload);
            assert_eq!(writer.calls, 1);
            assert_eq!(
                outcome.disposition(),
                Some(GuardianInputDisposition::KnownNotApplied)
            );
            assert_eq!(outcome.applied_bytes(), Some(0));
        }

        let mut mismatched = Writer {
            mode: WriteMode::Full,
            calls: 0,
        };
        let outcome = permit().write_once(&mut mismatched, b"abce");
        assert_eq!(mismatched.calls, 0);
        assert!(!outcome.writer_was_invoked());
        assert_eq!(
            outcome.disposition(),
            Some(GuardianInputDisposition::KnownNotApplied)
        );

        let mut invalid = Writer {
            mode: WriteMode::OverReported,
            calls: 0,
        };
        let outcome = permit().write_once(&mut invalid, payload);
        assert_eq!(invalid.calls, 1);
        assert_eq!(outcome.disposition(), None);
        assert_eq!(outcome.applied_bytes(), None);
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize journal");
        assert_eq!(journal.guardian_incarnation(), guardian());
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
        assert_eq!(
            durable_permit.protocol_state(),
            InputEffectState::DurableFull
        );
        drop(journal);

        let physical = std::fs::read(&path).expect("read physical bytes");
        assert!(!physical
            .windows(32)
            .any(|window| window == [0xa7; 32].as_slice()));
        assert!(!physical
            .windows(16)
            .any(|window| window == [0x61; 16].as_slice()));

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover journal");
        {
            let recovered = reopened
                .effect(input.effect_id())
                .expect("effect recovered");
            assert_eq!(recovered.identity(), input);
            assert_eq!(
                recovered.disposition(),
                GuardianInputDisposition::DurableFull
            );
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
                .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
            guardian(),
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("recover exact durable prefix");
        let recovered = reopened
            .effect(input.effect_id())
            .expect("effect recovered");
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
                .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
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
    fn header_only_reopen_withholds_append_authority() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("header-only.input.journal");
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize header-only journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate header-only journal");
        assert_eq!(journal.record_count(), 0);
        drop(journal);

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("scan header-only journal");
        assert_eq!(reopened.record_count(), 0);
        assert!(matches!(
            reopened.append_intent_and_sync(identity(7, 0x6a, 0xaf)),
            Err(GuardianInputJournalError::RecoveryAuthorityUnavailable)
        ));
        assert_eq!(reopened.record_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn phase_scan_is_conservative_and_recovery_append_authority_is_withheld() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let intent_path = temp.path().join("intent.input.journal");
        let input = identity(7, 0x6c, 0xb1);
        let mut journal = GuardianInputJournal::create(
            create_file(&intent_path),
            pane(),
            guardian(),
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
            open_file(&intent_path),
            pane(),
            guardian(),
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
            InputEffectState::DispositionUnavailable
        );
        assert!(matches!(
            journal
                .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable,),
            Err(GuardianInputJournalError::RecoveryAuthorityUnavailable)
        ));
        drop(journal);

        let accepted_path = temp.path().join("accepted.input.journal");
        let mut journal = GuardianInputJournal::create(
            create_file(&accepted_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize accepted-phase journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate accepted-phase journal");
        journal
            .append_intent_and_sync(input)
            .expect("accepted-phase intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        drop(journal);

        let mut journal = GuardianInputJournal::open(
            open_file(&accepted_path),
            pane(),
            guardian(),
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
        assert!(matches!(
            journal.append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied,),
            Err(GuardianInputJournalError::RecoveryAuthorityUnavailable)
        ));
        drop(journal);

        let terminal_path = temp.path().join("terminal.input.journal");
        let mut journal = GuardianInputJournal::create(
            create_file(&terminal_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize terminal-phase journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate terminal-phase journal");
        journal
            .append_intent_and_sync(input)
            .expect("terminal-phase intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("terminal-phase accepted marker");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::KnownNotApplied)
            .expect("terminal-phase zero-byte resolution");
        drop(journal);

        let mut journal = GuardianInputJournal::open(
            open_file(&terminal_path),
            pane(),
            guardian(),
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
        assert!(matches!(
            journal.append_intent_and_sync(identity(8, 0x6f, 0xb4)),
            Err(GuardianInputJournalError::RecoveryAuthorityUnavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn valid_intent_prefix_rollback_never_becomes_replay_authority() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let complete_path = temp.path().join("complete.input.journal");
        let rollback_path = temp.path().join("rolled-back.input.journal");
        let input = identity(7, 0x70, 0xb5);
        let mut journal = GuardianInputJournal::create(
            create_file(&complete_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize complete journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate complete journal");
        journal.append_intent_and_sync(input).expect("intent");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
            .expect("accepted marker");
        journal
            .append_disposition_and_sync(input, GuardianInputDisposition::DurableFull)
            .expect("terminal marker");
        drop(journal);

        let complete = std::fs::read(&complete_path).expect("read complete journal");
        let valid_intent_prefix = FILE_HEADER_BYTES + RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES;
        let mut rollback = create_file(&rollback_path);
        rollback
            .write_all(&complete[..valid_intent_prefix])
            .expect("write valid rolled-back prefix");
        rollback.sync_all().expect("sync valid rolled-back prefix");
        drop(rollback);

        let mut recovered = GuardianInputJournal::open(
            open_file(&rollback_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("scan valid rolled-back prefix");
        assert_eq!(
            recovered
                .effect(input.effect_id())
                .expect("rolled-back intent retained")
                .disposition()
                .recovery_protocol_state(),
            InputEffectState::DispositionUnavailable
        );
        assert!(matches!(
            recovered
                .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable,),
            Err(GuardianInputJournalError::RecoveryAuthorityUnavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn every_partial_record_framing_cut_preserves_only_the_committed_prefix() {
        let complete = tempfile::tempdir().expect("create complete fixture tempdir");
        let complete_path = complete.path().join("input.journal");
        let input = identity(7, 0x6d, 0xb2);
        let mut journal = GuardianInputJournal::create(
            create_file(&complete_path),
            pane(),
            guardian(),
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
            let path = cuts_dir
                .path()
                .join(format!("cut-{trailing_bytes}.journal"));
            let cut = committed_prefix + trailing_bytes;
            let mut file = create_file(&path);
            file.write_all(&physical[..cut]).expect("write crash cut");
            file.sync_all().expect("sync crash cut fixture");
            drop(file);

            let mut recovered = GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
                cipher(),
                GuardianInputJournalLimits::default(),
            )
            .expect("recover committed prefix before partial marker");
            assert_eq!(
                recovered.tail(),
                GuardianInputJournalTail::Incomplete {
                    committed_bytes: u64::try_from(committed_prefix)
                        .expect("fixture prefix fits u64"),
                    trailing_bytes: u64::try_from(trailing_bytes).expect("fixture tail fits u64"),
                }
            );
            let effect = recovered
                .effect(input.effect_id())
                .expect("intent retained");
            assert_eq!(effect.disposition(), GuardianInputDisposition::Intent);
            assert_eq!(
                effect.disposition().recovery_protocol_state(),
                InputEffectState::DispositionUnavailable
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
        let mut journal = GuardianInputJournal::create(
            create_file(&complete_path),
            pane(),
            guardian(),
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
        let accepted_prefix =
            FILE_HEADER_BYTES + 2 * (RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES);
        let cuts = tempfile::tempdir().expect("create terminal crash-cut tempdir");
        for trailing_bytes in 1..(RECORD_HEADER_BYTES + RECORD_CIPHERTEXT_BYTES) {
            let path = cuts
                .path()
                .join(format!("terminal-cut-{trailing_bytes}.journal"));
            let cut = accepted_prefix + trailing_bytes;
            let mut file = create_file(&path);
            file.write_all(&physical[..cut])
                .expect("write terminal crash cut");
            file.sync_all().expect("sync terminal crash cut");
            drop(file);

            let mut recovered = GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
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
            let effect = recovered
                .effect(input.effect_id())
                .expect("accepted effect retained");
            assert_eq!(
                effect.disposition(),
                GuardianInputDisposition::AcceptedNotDurable
            );
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
            let mut journal = GuardianInputJournal::create(
                create_file(&path),
                pane(),
                guardian(),
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
                guardian(),
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
                    assert_eq!(recovered_disposition, GuardianInputDisposition::Intent);
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
                    assert_eq!(recovered_disposition, GuardianInputDisposition::Intent);
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
        physical
            .write_all(&RECORD_MAGIC[..4])
            .expect("write torn tail");
        physical.sync_all().expect("sync torn tail");
        drop(physical);

        let mut reopened = GuardianInputJournal::open(
            open_file(&path),
            pane(),
            guardian(),
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
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").len(),
            committed + 4
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrong_key_and_complete_corruption_fail_closed() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x65, 0xab);
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
                Uuid::from_bytes([0x32; 16]),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::GuardianIncarnationMismatch)
        ));
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
                GuardianOutputCipher::try_from_key_slice(&[0x74; 32]).expect("wrong key valid"),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::KeyIdentityMismatch)
        ));

        let mut file = open_file(&path);
        file.seek(SeekFrom::Start(
            FILE_HEADER_BYTES_U64 + RECORD_HEADER_BYTES_U64,
        ))
        .expect("seek ciphertext");
        file.write_all(&[0xff]).expect("corrupt ciphertext");
        file.sync_all().expect("sync corruption");
        drop(file);
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
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
        let complete_header = encode_file_header(pane(), guardian(), cipher().key_id());
        let mut file = create_file(&path);
        file.write_all(&complete_header[..FILE_HEADER_BYTES - 1])
            .expect("write torn header");
        file.sync_all().expect("sync torn header");
        drop(file);
        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
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
            std::fs::metadata(&path)
                .expect("torn header metadata")
                .len(),
            u64::try_from(FILE_HEADER_BYTES - 1).expect("fixture header length fits u64")
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_v1_journal_is_preserved_and_rejected_as_ambiguous() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let mut legacy_header = encode_file_header(pane(), guardian(), cipher().key_id());
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
                guardian(),
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
    fn legacy_v2_journal_without_guardian_binding_is_preserved_and_rejected() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let mut legacy_header = encode_file_header(pane(), guardian(), cipher().key_id());
        legacy_header[..8].copy_from_slice(&LEGACY_FILE_MAGIC_V2);
        legacy_header[8..12].copy_from_slice(&2_u32.to_le_bytes());
        let mut file = create_file(&path);
        file.write_all(&legacy_header)
            .expect("write legacy v2 header");
        file.sync_all().expect("sync legacy v2 header");
        drop(file);

        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                guardian(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::LegacyV2GuardianIdentityUnbound)
        ));
        let preserved = std::fs::read(&path).expect("read preserved legacy v2 header");
        assert_eq!(preserved.as_slice(), legacy_header.as_slice());
    }

    #[cfg(unix)]
    #[test]
    fn recomputed_outer_digest_cannot_bypass_aead_authentication() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x6b, 0xb0);
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
            guardian(),
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
                guardian(),
                cipher(),
                GuardianInputJournalLimits::default(),
            ),
            Err(GuardianInputJournalError::RecordAuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rewritten_header_and_outer_digest_cannot_transplant_records_across_incarnations() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let input = identity(7, 0x75, 0xbb);
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize source-incarnation journal");
        journal
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate source-incarnation journal");
        journal
            .append_intent_and_sync(input)
            .expect("source intent");
        drop(journal);

        let successor_incarnation = Uuid::from_bytes([0x32; 16]);
        let mut physical = std::fs::read(&path).expect("read source journal");
        physical[40..56].copy_from_slice(successor_incarnation.as_bytes());
        let mut header_hasher = Sha256::new();
        header_hasher.update(FILE_DIGEST_DOMAIN);
        header_hasher.update(&physical[..96]);
        physical[96..128].copy_from_slice(&header_hasher.finalize());

        let record_offset = FILE_HEADER_BYTES;
        let ciphertext_offset = record_offset + RECORD_HEADER_BYTES;
        let mut prefix = [0_u8; 32];
        prefix.copy_from_slice(&physical[record_offset..record_offset + 32]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&physical[record_offset + 32..record_offset + 56]);
        let digest = record_digest(
            pane(),
            successor_incarnation,
            &prefix,
            &nonce,
            &physical[ciphertext_offset..ciphertext_offset + RECORD_CIPHERTEXT_BYTES],
        );
        physical[record_offset + 56..record_offset + 88].copy_from_slice(&digest);
        let mut file = open_file(&path);
        file.seek(SeekFrom::Start(0))
            .expect("seek transplanted journal");
        file.write_all(&physical)
            .expect("write transplanted journal");
        file.sync_all().expect("sync transplanted journal");
        drop(file);

        assert!(matches!(
            GuardianInputJournal::open(
                open_file(&path),
                pane(),
                successor_incarnation,
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
                .append_disposition_and_sync(input, GuardianInputDisposition::AcceptedNotDurable)
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
        source_ciphertext
            .copy_from_slice(&physical[source + RECORD_HEADER_BYTES..source + record_bytes]);
        physical[destination + 32..destination + 56].copy_from_slice(&source_nonce);
        physical[destination + RECORD_HEADER_BYTES..destination + record_bytes]
            .copy_from_slice(&source_ciphertext);

        let mut destination_prefix = [0_u8; 32];
        destination_prefix.copy_from_slice(&physical[destination..destination + 32]);
        let digest = record_digest(
            pane(),
            guardian(),
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
                guardian(),
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
        let mut journal = GuardianInputJournal::create(
            create_file(&path),
            pane(),
            guardian(),
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
    fn activation_identity_and_full_lifecycle_capacity_fail_closed() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let path = temp.path().join("input.journal");
        let limits = GuardianInputJournalLimits {
            max_log_bytes: FILE_HEADER_BYTES_U64 + 2 * RECORD_BYTES_U64,
            max_records: 2,
            max_effects: 2,
            max_input_bytes: 4,
        };
        let mut journal =
            GuardianInputJournal::create(create_file(&path), pane(), guardian(), cipher(), limits)
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
        assert!(matches!(
            journal.append_intent_and_sync(first),
            Err(GuardianInputJournalError::RecordLimit { maximum: 2 })
        ));
        assert_eq!(journal.record_count(), 0);
        assert!(journal.effect(first.effect_id()).is_none());

        let byte_limited_path = temp.path().join("byte-limited.input.journal");
        let byte_limit = FILE_HEADER_BYTES_U64 + 2 * RECORD_BYTES_U64;
        let mut byte_limited = GuardianInputJournal::create(
            create_file(&byte_limited_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits {
                max_log_bytes: byte_limit,
                max_records: 3,
                max_effects: 2,
                max_input_bytes: 4,
            },
        )
        .expect("initialize byte-limited journal");
        byte_limited
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate byte-limited journal");
        assert!(matches!(
            byte_limited.append_intent_and_sync(first),
            Err(GuardianInputJournalError::LogByteLimit { observed, maximum })
                if observed == FILE_HEADER_BYTES_U64 + 3 * RECORD_BYTES_U64
                    && maximum == byte_limit
        ));
        assert_eq!(byte_limited.record_count(), 0);
        assert!(byte_limited.effect(first.effect_id()).is_none());

        let sequence_path = temp.path().join("sequence-limited.input.journal");
        let mut sequence_limited = GuardianInputJournal::create(
            create_file(&sequence_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits::default(),
        )
        .expect("initialize sequence-limited journal");
        sequence_limited
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate sequence-limited journal");
        sequence_limited.next_journal_sequence = Some(u64::MAX - 1);
        assert!(matches!(
            sequence_limited.append_intent_and_sync(first),
            Err(GuardianInputJournalError::JournalSequenceExhausted)
        ));
        assert_eq!(sequence_limited.record_count(), 0);
        assert!(sequence_limited.effect(first.effect_id()).is_none());

        let promised_path = temp.path().join("promised-followup.input.journal");
        let mut promised = GuardianInputJournal::create(
            create_file(&promised_path),
            pane(),
            guardian(),
            cipher(),
            GuardianInputJournalLimits {
                max_log_bytes: FILE_HEADER_BYTES_U64 + 5 * RECORD_BYTES_U64,
                max_records: 5,
                max_effects: 2,
                max_input_bytes: 4,
            },
        )
        .expect("initialize promised-followup journal");
        promised
            .sync_parent_directory_and_activate(&open_directory(temp.path()))
            .expect("activate promised-followup journal");
        promised
            .append_intent_and_sync(first)
            .expect("first intent reserves its remaining lifecycle");
        let second = identity(8, 0x69, 0xaf);
        assert!(matches!(
            promised.append_intent_and_sync(second),
            Err(GuardianInputJournalError::RecordLimit { maximum: 5 })
        ));
        assert_eq!(promised.record_count(), 1);
        assert_eq!(
            promised
                .effect(first.effect_id())
                .expect("first reservation remains")
                .disposition(),
            GuardianInputDisposition::Intent
        );
        assert!(promised.effect(second.effect_id()).is_none());
    }
}
