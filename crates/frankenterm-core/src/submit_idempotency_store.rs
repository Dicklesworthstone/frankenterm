//! Durable, fail-closed claim state machine for verified-submit idempotency.
//!
//! The caller nonce selects one stable row. A separately stored semantic
//! request digest makes nonce reuse with changed text or verification semantics
//! a typed conflict rather than a second effect. The owner state is split from
//! `effect_applied_receipt_pending` and `in_doubt`, so replay never treats a
//! successful injector call as an unperformed write merely because later wait,
//! audit, or receipt work was interrupted. This is an at-most-once automatic
//! effect contract; reconciliation remains an explicit future operation.

use crate::robot_types::{SubmitGuaranteeLevel, SubmitReceipt, SubmitReceiptState};
use crate::verified_submit::SubmitIdempotencyBinding;
use rand::{TryRng, rngs::SysRng};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STORE_FILENAME: &str = "submit_idempotency.sqlite3";
const LEGACY_STORE_NAME: &str = "submit_idempotency";
const STORE_APPLICATION_ID: i64 = 0x4654_4944;
const STORE_SCHEMA_VERSION: i64 = 2;
const RECEIPT_SCHEMA_VERSION: u16 = 1;
const STATE_ACTIVE_OWNER: i64 = 1;
const STATE_EFFECT_APPLIED_RECEIPT_PENDING: i64 = 2;
const STATE_IN_DOUBT: i64 = 3;
const STATE_COMPLETED: i64 = 4;
const STATE_RETRYABLE: i64 = 5;
const RETRYABLE_POLICY_DENIED: i64 = 1;
const RETRYABLE_APPROVAL_REQUIRED: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const OWNER_NONCE_BYTES: usize = 32;
const OWNER_LEASE_DURATION_MS: i64 = 60_000;
const MAX_RECEIPT_JSON_BYTES: usize = 64 * 1024;
const MAX_RECEIPT_EVIDENCE_ITEMS: usize = 64;
const MAX_RECEIPT_FIELD_BYTES: usize = 1024;
const MAX_RECEIPT_CURSOR_BYTES: usize = 512;
const MAX_CALLER_KEY_BYTES: usize = 256;
const MAX_STORE_RECORDS: i64 = 16_384;
const MAX_STORE_LOGICAL_BYTES: i64 = 128 * 1024 * 1024;
const LOGICAL_RECORD_OVERHEAD_BYTES: i64 = 128;

const CREATE_TABLE_SQL: &str = "CREATE TABLE verified_submit_idempotency (idempotency_key TEXT COLLATE BINARY PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL, pane_id TEXT COLLATE BINARY NOT NULL, request_sha256 TEXT COLLATE BINARY NOT NULL, effect_sha256 TEXT COLLATE BINARY NOT NULL, state INTEGER NOT NULL CHECK (state IN (1, 2, 3, 4, 5)), retryable_reason INTEGER, receipt_json TEXT COLLATE BINARY, generation INTEGER NOT NULL CHECK (generation >= 1), owner_nonce BLOB NOT NULL CHECK (typeof(owner_nonce) = 'blob' AND length(owner_nonce) = 32), lease_expires_unix_ms INTEGER CHECK (lease_expires_unix_ms IS NULL OR lease_expires_unix_ms >= 0), created_unix_ms INTEGER NOT NULL, updated_unix_ms INTEGER NOT NULL, CHECK (length(CAST(idempotency_key AS BLOB)) BETWEEN 71 AND 90), CHECK (length(CAST(pane_id AS BLOB)) BETWEEN 1 AND 20), CHECK (length(CAST(request_sha256 AS BLOB)) = 64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (length(CAST(effect_sha256 AS BLOB)) = 64 AND effect_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (receipt_json IS NULL OR length(CAST(receipt_json AS BLOB)) <= 65536), CHECK ((state = 1 AND retryable_reason IS NULL AND receipt_json IS NULL AND lease_expires_unix_ms IS NOT NULL) OR (state IN (2, 3) AND retryable_reason IS NULL AND receipt_json IS NULL AND lease_expires_unix_ms IS NULL) OR (state = 4 AND retryable_reason IS NULL AND receipt_json IS NOT NULL AND lease_expires_unix_ms IS NULL) OR (state = 5 AND retryable_reason IN (1, 2) AND receipt_json IS NULL AND lease_expires_unix_ms IS NULL))) STRICT, WITHOUT ROWID";
const CREATE_INDEX_SQL: &str = "CREATE INDEX verified_submit_idempotency_request_lookup ON verified_submit_idempotency (pane_id COLLATE BINARY, request_sha256 COLLATE BINARY)";

/// Finite failure taxonomy. No variant retains a filesystem path, SQL string,
/// serialized receipt, or backend error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitIdempotencyError {
    #[error("submit idempotency binding is invalid")]
    InvalidBinding,
    #[error("submit idempotency caller key is empty")]
    EmptyCallerKey,
    #[error("submit idempotency database path is a symbolic link")]
    SymlinkRejected,
    #[error("legacy submit idempotency storage is present")]
    LegacyStorePresent,
    #[error("submit idempotency database directory is unavailable")]
    DirectoryUnavailable,
    #[error("submit idempotency database open failed")]
    OpenFailed,
    #[error("submit idempotency database is busy")]
    Busy,
    #[error("submit idempotency database configuration failed")]
    ConfigurationFailed,
    #[error("submit idempotency schema is unsupported")]
    SchemaMismatch,
    #[error("submit idempotency request conflicts with the caller key")]
    RequestConflict,
    #[error("submit idempotency store capacity is exhausted")]
    CapacityExceeded,
    #[error("submit idempotency owner entropy is unavailable")]
    EntropyUnavailable,
    #[error("submit idempotency claim failed")]
    ClaimFailed,
    #[error("submit idempotency transition failed")]
    TransitionFailed,
    #[error("submit idempotency claim is missing")]
    MissingClaim,
    #[error("submit idempotency transition is invalid")]
    InvalidTransition,
    #[error("submit idempotency record is corrupt")]
    RecordCorrupt,
    #[error("submit idempotency receipt exceeds its storage bound")]
    ReceiptOversize,
    #[error("submit idempotency receipt is invalid")]
    ReceiptInvalid,
}

impl SubmitIdempotencyError {
    /// Stable, content-free class suitable for logs and external error mapping.
    #[must_use]
    pub const fn error_class(self) -> &'static str {
        match self {
            Self::InvalidBinding => "invalid_binding",
            Self::EmptyCallerKey => "empty_caller_key",
            Self::SymlinkRejected => "symlink_rejected",
            Self::LegacyStorePresent => "legacy_store_present",
            Self::DirectoryUnavailable => "directory_unavailable",
            Self::OpenFailed => "open_failed",
            Self::Busy => "busy",
            Self::ConfigurationFailed => "configuration_failed",
            Self::SchemaMismatch => "schema_mismatch",
            Self::RequestConflict => "request_conflict",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::EntropyUnavailable => "entropy_unavailable",
            Self::ClaimFailed => "claim_failed",
            Self::TransitionFailed => "transition_failed",
            Self::MissingClaim => "missing_claim",
            Self::InvalidTransition => "invalid_transition",
            Self::RecordCorrupt => "record_corrupt",
            Self::ReceiptOversize => "receipt_oversize",
            Self::ReceiptInvalid => "receipt_invalid",
        }
    }

    /// Only a real open failure or SQLite lock/busy result is retryable.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::OpenFailed | Self::Busy)
    }
}

/// A successful unique claim, a completed replay, or a conservative refusal to
/// retry an owner whose side-effect outcome is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ClaimOutcome {
    Claimed(ClaimToken),
    Completed(SubmitReceipt),
    InFlight,
    EffectAppliedReceiptPending,
    InDoubt,
}

/// Opaque ownership generation returned by [`claim`]. Every terminal
/// transition must present the token so a stale owner cannot complete or reopen
/// a later claimant's generation (the retryable-state ABA guard).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClaimToken {
    generation: i64,
    owner_nonce: [u8; OWNER_NONCE_BYTES],
}

impl fmt::Debug for ClaimToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaimToken")
            .field("generation", &self.generation)
            .field("owner_nonce", &"[REDACTED]")
            .finish()
    }
}

/// Pre-effect terminal outcomes that are proven safe to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryableReason {
    PolicyDenied,
    ApprovalRequired,
}

impl RetryableReason {
    const fn as_db(self) -> i64 {
        match self {
            Self::PolicyDenied => RETRYABLE_POLICY_DENIED,
            Self::ApprovalRequired => RETRYABLE_APPROVAL_REQUIRED,
        }
    }

    fn from_db(value: i64) -> Result<Self, SubmitIdempotencyError> {
        match value {
            RETRYABLE_POLICY_DENIED => Ok(Self::PolicyDenied),
            RETRYABLE_APPROVAL_REQUIRED => Ok(Self::ApprovalRequired),
            _ => Err(SubmitIdempotencyError::RecordCorrupt),
        }
    }
}

/// Read-only state used by diagnostics and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredSubmitState {
    ActiveOwner,
    EffectAppliedReceiptPending,
    InDoubt,
    Completed(SubmitReceipt),
    Retryable(RetryableReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceiptEnvelopeV1 {
    schema_version: u16,
    internal_claim_key: String,
    pane_id: u64,
    request_sha256: String,
    effect_sha256: String,
    receipt: StoredSubmitReceiptV1,
}

/// Storage-local mirror so unknown/missing receipt fields cannot be silently
/// accepted by serde when reading an authority-bearing completed row. Any
/// change to this shape requires a store-schema migration/version bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSubmitReceiptV1 {
    state: StoredSubmitReceiptStateV1,
    guarantee_level: StoredSubmitGuaranteeLevelV1,
    guarantee_met: bool,
    agent_type: Option<String>,
    profile_id: Option<String>,
    profile_version: Option<String>,
    attempts: u32,
    evidence_rule_ids: Vec<String>,
    elapsed_ms: u64,
    polls: usize,
    cursor_before: Option<String>,
    cursor_after: Option<String>,
    idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredSubmitReceiptStateV1 {
    Submitted,
    QueuedBehindOperation,
    StuckInComposer,
    PaneCrashedToShell,
    VerificationUnavailable,
    PolicyDenied,
    RequiresApproval,
    SendFailed,
}

impl From<SubmitReceiptState> for StoredSubmitReceiptStateV1 {
    fn from(state: SubmitReceiptState) -> Self {
        match state {
            SubmitReceiptState::Submitted => Self::Submitted,
            SubmitReceiptState::QueuedBehindOperation => Self::QueuedBehindOperation,
            SubmitReceiptState::StuckInComposer => Self::StuckInComposer,
            SubmitReceiptState::PaneCrashedToShell => Self::PaneCrashedToShell,
            SubmitReceiptState::VerificationUnavailable => Self::VerificationUnavailable,
            SubmitReceiptState::PolicyDenied => Self::PolicyDenied,
            SubmitReceiptState::RequiresApproval => Self::RequiresApproval,
            SubmitReceiptState::SendFailed => Self::SendFailed,
        }
    }
}

impl From<StoredSubmitReceiptStateV1> for SubmitReceiptState {
    fn from(state: StoredSubmitReceiptStateV1) -> Self {
        match state {
            StoredSubmitReceiptStateV1::Submitted => Self::Submitted,
            StoredSubmitReceiptStateV1::QueuedBehindOperation => Self::QueuedBehindOperation,
            StoredSubmitReceiptStateV1::StuckInComposer => Self::StuckInComposer,
            StoredSubmitReceiptStateV1::PaneCrashedToShell => Self::PaneCrashedToShell,
            StoredSubmitReceiptStateV1::VerificationUnavailable => Self::VerificationUnavailable,
            StoredSubmitReceiptStateV1::PolicyDenied => Self::PolicyDenied,
            StoredSubmitReceiptStateV1::RequiresApproval => Self::RequiresApproval,
            StoredSubmitReceiptStateV1::SendFailed => Self::SendFailed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredSubmitGuaranteeLevelV1 {
    Write,
    Composer,
    Submitted,
    Working,
}

impl From<SubmitGuaranteeLevel> for StoredSubmitGuaranteeLevelV1 {
    fn from(level: SubmitGuaranteeLevel) -> Self {
        match level {
            SubmitGuaranteeLevel::Write => Self::Write,
            SubmitGuaranteeLevel::Composer => Self::Composer,
            SubmitGuaranteeLevel::Submitted => Self::Submitted,
            SubmitGuaranteeLevel::Working => Self::Working,
        }
    }
}

impl From<StoredSubmitGuaranteeLevelV1> for SubmitGuaranteeLevel {
    fn from(level: StoredSubmitGuaranteeLevelV1) -> Self {
        match level {
            StoredSubmitGuaranteeLevelV1::Write => Self::Write,
            StoredSubmitGuaranteeLevelV1::Composer => Self::Composer,
            StoredSubmitGuaranteeLevelV1::Submitted => Self::Submitted,
            StoredSubmitGuaranteeLevelV1::Working => Self::Working,
        }
    }
}

impl From<&SubmitReceipt> for StoredSubmitReceiptV1 {
    fn from(receipt: &SubmitReceipt) -> Self {
        Self {
            state: receipt.state.into(),
            guarantee_level: receipt.guarantee_level.into(),
            guarantee_met: receipt.guarantee_met,
            agent_type: receipt.agent_type.clone(),
            profile_id: receipt.profile_id.clone(),
            profile_version: receipt.profile_version.clone(),
            attempts: receipt.attempts,
            evidence_rule_ids: receipt.evidence_rule_ids.clone(),
            elapsed_ms: receipt.elapsed_ms,
            polls: receipt.polls,
            cursor_before: receipt.cursor_before.clone(),
            cursor_after: receipt.cursor_after.clone(),
            idempotency_key: receipt.idempotency_key.clone(),
        }
    }
}

impl From<StoredSubmitReceiptV1> for SubmitReceipt {
    fn from(receipt: StoredSubmitReceiptV1) -> Self {
        Self {
            state: receipt.state.into(),
            guarantee_level: receipt.guarantee_level.into(),
            guarantee_met: receipt.guarantee_met,
            agent_type: receipt.agent_type,
            profile_id: receipt.profile_id,
            profile_version: receipt.profile_version,
            attempts: receipt.attempts,
            evidence_rule_ids: receipt.evidence_rule_ids,
            elapsed_ms: receipt.elapsed_ms,
            polls: receipt.polls,
            cursor_before: receipt.cursor_before,
            cursor_after: receipt.cursor_after,
            idempotency_key: receipt.idempotency_key,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StoredHeader {
    state: i64,
    retryable_reason: Option<i64>,
    generation: i64,
    owner_nonce: [u8; OWNER_NONCE_BYTES],
    lease_expires_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct StoreLimits {
    max_records: i64,
    max_logical_bytes: i64,
    receipt_reserve_bytes: i64,
}

const PRODUCTION_LIMITS: StoreLimits = StoreLimits {
    max_records: MAX_STORE_RECORDS,
    max_logical_bytes: MAX_STORE_LOGICAL_BYTES,
    receipt_reserve_bytes: MAX_RECEIPT_JSON_BYTES as i64,
};

#[derive(Debug, Clone, Copy)]
enum StoreOpenMode {
    Create,
    Existing,
}

/// Location of the dedicated SQLite store.
#[must_use]
pub fn database_path(ft_dir: &Path) -> PathBuf {
    ft_dir.join(STORE_FILENAME)
}

/// Whether a key has the exact canonical full-digest form
/// `idem:<canonical-u64>:<64-lowercase-hex>`.
#[must_use]
pub fn is_valid_submit_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("idem:") else {
        return false;
    };
    let Some((pane, digest)) = rest.split_once(':') else {
        return false;
    };
    let pane_ok = !pane.is_empty()
        && pane.len() <= 20
        && pane.bytes().all(|byte| byte.is_ascii_digit())
        && (pane == "0" || !pane.starts_with('0'))
        && pane.parse::<u64>().is_ok();
    let digest_ok = digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    pane_ok && digest_ok
}

fn validate_binding(binding: &SubmitIdempotencyBinding) -> Result<(), SubmitIdempotencyError> {
    if binding.caller_key().is_empty() {
        return Err(SubmitIdempotencyError::EmptyCallerKey);
    }
    if binding.caller_key().len() > MAX_CALLER_KEY_BYTES || !binding.is_canonical() {
        return Err(SubmitIdempotencyError::InvalidBinding);
    }
    let pane_matches = binding
        .key()
        .strip_prefix("idem:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(pane, _)| pane.parse::<u64>().ok())
        == Some(binding.pane_id());
    if is_valid_submit_key(binding.key()) && pane_matches {
        Ok(())
    } else {
        Err(SubmitIdempotencyError::InvalidBinding)
    }
}

fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn map_sqlite_error(
    error: &rusqlite::Error,
    fallback: SubmitIdempotencyError,
) -> SubmitIdempotencyError {
    match error.sqlite_error_code() {
        Some(rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked) => {
            SubmitIdempotencyError::Busy
        }
        _ => fallback,
    }
}

fn map_sqlite<T>(
    result: rusqlite::Result<T>,
    fallback: SubmitIdempotencyError,
) -> Result<T, SubmitIdempotencyError> {
    result.map_err(|error| map_sqlite_error(&error, fallback))
}

fn schema_header(conn: &Connection) -> Result<(i64, i64), SubmitIdempotencyError> {
    let application_id = map_sqlite(
        conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let user_version = map_sqlite(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    Ok((application_id, user_version))
}

fn validate_initialized_schema_locked(
    conn: &Connection,
) -> Result<(), SubmitIdempotencyError> {
    if schema_header(conn)? != (STORE_APPLICATION_ID, STORE_SCHEMA_VERSION) {
        return Err(SubmitIdempotencyError::SchemaMismatch);
    }

    let (table_sql, index_sql, user_objects) = map_sqlite(
        conn.query_row(
            "SELECT (SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = 'verified_submit_idempotency' COLLATE BINARY), (SELECT sql FROM main.sqlite_schema WHERE type = 'index' AND name = 'verified_submit_idempotency_request_lookup' COLLATE BINARY), (SELECT COUNT(*) FROM main.sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    if table_sql.as_deref() != Some(CREATE_TABLE_SQL)
        || index_sql.as_deref() != Some(CREATE_INDEX_SQL)
        || user_objects != 2
    {
        return Err(SubmitIdempotencyError::SchemaMismatch);
    }

    let (without_rowid, strict) = map_sqlite(
        conn
        .query_row(
            "SELECT wr, strict FROM pragma_table_list \
             WHERE schema = 'main' AND name = 'verified_submit_idempotency'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (columns, matching_columns) = map_sqlite(
        conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE \
                 WHEN cid = 0 AND name = 'idempotency_key' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 1 AND hidden = 0 THEN 1 \
                 WHEN cid = 1 AND name = 'schema_version' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 2 AND name = 'pane_id' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 3 AND name = 'request_sha256' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 4 AND name = 'effect_sha256' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 5 AND name = 'state' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 6 AND name = 'retryable_reason' AND type = 'INTEGER' AND \"notnull\" = 0 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 7 AND name = 'receipt_json' AND type = 'TEXT' AND \"notnull\" = 0 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 8 AND name = 'generation' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 9 AND name = 'owner_nonce' AND type = 'BLOB' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 10 AND name = 'lease_expires_unix_ms' AND type = 'INTEGER' AND \"notnull\" = 0 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 11 AND name = 'created_unix_ms' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 12 AND name = 'updated_unix_ms' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 ELSE 0 END), 0) \
             FROM pragma_table_xinfo('verified_submit_idempotency')",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (triggers, foreign_keys, indexes) = map_sqlite(
        conn
        .query_row(
            "SELECT \
                 (SELECT COUNT(*) FROM main.sqlite_schema \
                  WHERE type = 'trigger' AND tbl_name = 'verified_submit_idempotency'), \
                 (SELECT COUNT(*) \
                  FROM pragma_foreign_key_list('verified_submit_idempotency')), \
                 (SELECT COUNT(*) FROM pragma_index_list('verified_submit_idempotency'))",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (index_unique, index_origin, index_partial) = map_sqlite(
        conn
        .query_row(
            "SELECT \"unique\", origin, partial \
             FROM pragma_index_list('verified_submit_idempotency') \
             WHERE name = 'verified_submit_idempotency_request_lookup'",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (index_columns, matching_index_columns) = map_sqlite(
        conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE \
                 WHEN seqno = 0 AND cid = 2 AND name = 'pane_id' AND \"desc\" = 0 AND coll = 'BINARY' THEN 1 \
                 WHEN seqno = 1 AND cid = 3 AND name = 'request_sha256' AND \"desc\" = 0 AND coll = 'BINARY' THEN 1 \
                 ELSE 0 END), 0) \
             FROM pragma_index_xinfo('verified_submit_idempotency_request_lookup') WHERE key = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (pk_name, pk_unique, pk_partial) = map_sqlite(
        conn.query_row(
            "SELECT name, \"unique\", partial FROM pragma_index_list('verified_submit_idempotency') WHERE origin = 'pk'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    let (pk_columns, matching_pk_columns) = map_sqlite(
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN seqno = 0 AND cid = 0 AND name = 'idempotency_key' AND \"desc\" = 0 AND coll = 'BINARY' THEN 1 ELSE 0 END), 0) FROM pragma_index_xinfo(?1) WHERE key = 1",
            [pk_name],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    if without_rowid == 1
        && strict == 1
        && columns == 13
        && matching_columns == 13
        && index_unique == 0
        && index_origin == "c"
        && index_partial == 0
        && index_columns == 2
        && matching_index_columns == 2
        && pk_unique == 1
        && pk_partial == 0
        && pk_columns == 1
        && matching_pk_columns == 1
        && triggers == 0
        && foreign_keys == 0
        && indexes == 2
    {
        Ok(())
    } else {
        Err(SubmitIdempotencyError::SchemaMismatch)
    }
}

fn initialize_or_validate_schema_locked(
    conn: &Connection,
    allow_initialize: bool,
) -> Result<(), SubmitIdempotencyError> {
    let header = schema_header(conn)?;
    if header == (STORE_APPLICATION_ID, STORE_SCHEMA_VERSION) {
        return validate_initialized_schema_locked(conn);
    }
    if header != (0, 0) || !allow_initialize {
        return Err(SubmitIdempotencyError::SchemaMismatch);
    }
    let objects = map_sqlite(
        conn.query_row(
            "SELECT COUNT(*) FROM main.sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get::<_, i64>(0),
        ),
        SubmitIdempotencyError::SchemaMismatch,
    )?;
    if objects != 0 {
        return Err(SubmitIdempotencyError::SchemaMismatch);
    }
    map_sqlite(
        conn.execute_batch(CREATE_TABLE_SQL),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    map_sqlite(
        conn.execute_batch(CREATE_INDEX_SQL),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    map_sqlite(
        conn.pragma_update(None, "application_id", STORE_APPLICATION_ID),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    map_sqlite(
        conn.pragma_update(None, "user_version", STORE_SCHEMA_VERSION),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    validate_initialized_schema_locked(conn)
}

fn prepare_store_path(
    ft_dir: &Path,
    mode: StoreOpenMode,
) -> Result<Option<PathBuf>, SubmitIdempotencyError> {
    match std::fs::symlink_metadata(ft_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SubmitIdempotencyError::SymlinkRejected);
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(SubmitIdempotencyError::DirectoryUnavailable);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match mode {
            StoreOpenMode::Existing => return Ok(None),
            StoreOpenMode::Create => {
                std::fs::create_dir_all(ft_dir)
                    .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?;
                let metadata = std::fs::symlink_metadata(ft_dir)
                    .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?;
                if metadata.file_type().is_symlink() {
                    return Err(SubmitIdempotencyError::SymlinkRejected);
                }
                if !metadata.is_dir() {
                    return Err(SubmitIdempotencyError::DirectoryUnavailable);
                }
            }
        },
        Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
    }

    match std::fs::symlink_metadata(ft_dir.join(LEGACY_STORE_NAME)) {
        Ok(_) => return Err(SubmitIdempotencyError::LegacyStorePresent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
    }

    let path = database_path(ft_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SubmitIdempotencyError::SymlinkRejected);
        }
        Ok(metadata) if !metadata.is_file() => return Err(SubmitIdempotencyError::OpenFailed),
        Ok(_) => return Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match mode {
            StoreOpenMode::Create => return Ok(Some(path)),
            StoreOpenMode::Existing => return Ok(None),
        },
        Err(_) => return Err(SubmitIdempotencyError::OpenFailed),
    }
}

fn open_store(
    ft_dir: &Path,
    mode: StoreOpenMode,
) -> Result<Option<Connection>, SubmitIdempotencyError> {
    let Some(path) = prepare_store_path(ft_dir, mode)? else {
        return Ok(None);
    };
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if matches!(mode, StoreOpenMode::Create) {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    map_sqlite(
        conn.busy_timeout(BUSY_TIMEOUT),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    for (pragma, value) in [
        ("journal_mode", "WAL"),
        ("synchronous", "FULL"),
        ("fullfsync", "ON"),
        ("checkpoint_fullfsync", "ON"),
        ("trusted_schema", "OFF"),
        ("foreign_keys", "ON"),
    ] {
        map_sqlite(
            conn.pragma_update(None, pragma, value),
            SubmitIdempotencyError::ConfigurationFailed,
        )?;
    }
    Ok(Some(conn))
}

fn read_header(
    conn: &Connection,
    binding: &SubmitIdempotencyBinding,
    fallback: SubmitIdempotencyError,
) -> Result<Option<StoredHeader>, SubmitIdempotencyError> {
    let row = map_sqlite(
        conn
        .query_row(
            "SELECT schema_version, length(CAST(idempotency_key AS BLOB)), substr(CAST(idempotency_key AS BLOB), 1, 90), length(CAST(pane_id AS BLOB)), substr(CAST(pane_id AS BLOB), 1, 21), length(CAST(request_sha256 AS BLOB)), substr(CAST(request_sha256 AS BLOB), 1, 65), length(CAST(effect_sha256 AS BLOB)), substr(CAST(effect_sha256 AS BLOB), 1, 65), state, retryable_reason, length(CAST(receipt_json AS BLOB)), generation, length(owner_nonce), substr(owner_nonce, 1, 33) FROM verified_submit_idempotency WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY",
            [binding.key()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, Vec<u8>>(14)?,
                ))
            },
        )
        .optional(),
        fallback,
    )?;
    let Some((
        schema,
        key_len,
        key,
        pane_len,
        pane,
        digest_len,
        request_digest,
        effect_len,
        effect_digest,
        state,
        reason,
        bytes,
        generation,
        nonce_len,
        nonce,
    )) = row
    else {
        return Ok(None);
    };
    let expected_pane = binding.pane_id().to_string();
    if schema != STORE_SCHEMA_VERSION
        || key_len != i64::try_from(binding.key().len()).unwrap_or(i64::MAX)
        || key != binding.key().as_bytes()
        || pane_len != i64::try_from(expected_pane.len()).unwrap_or(i64::MAX)
        || pane != expected_pane.as_bytes()
        || digest_len != 64
        || request_digest.len() != 64
        || effect_len != 64
        || effect_digest.len() != 64
        || generation < 1
        || nonce_len != OWNER_NONCE_BYTES as i64
        || nonce.len() != OWNER_NONCE_BYTES
    {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    if request_digest != binding.request_sha256().as_bytes()
        || effect_digest != binding.effect_sha256().as_bytes()
    {
        return Err(SubmitIdempotencyError::RequestConflict);
    }
    let shape_ok = match state {
        STATE_ACTIVE_OWNER | STATE_EFFECT_APPLIED_RECEIPT_PENDING | STATE_IN_DOUBT => {
            reason.is_none() && bytes.is_none()
        }
        STATE_COMPLETED => {
            reason.is_none()
                && bytes.is_some_and(|value| {
                    value >= 0
                        && usize::try_from(value)
                            .is_ok_and(|size| size <= MAX_RECEIPT_JSON_BYTES)
                })
        }
        STATE_RETRYABLE => {
            bytes.is_none()
                && reason.is_some_and(|value| RetryableReason::from_db(value).is_ok())
        }
        _ => false,
    };
    if !shape_ok {
        return if bytes.is_some_and(|value| {
            usize::try_from(value).is_ok_and(|size| size > MAX_RECEIPT_JSON_BYTES)
        }) {
            Err(SubmitIdempotencyError::ReceiptOversize)
        } else {
            Err(SubmitIdempotencyError::RecordCorrupt)
        };
    }
    let owner_nonce: [u8; OWNER_NONCE_BYTES] = nonce
        .try_into()
        .map_err(|_| SubmitIdempotencyError::RecordCorrupt)?;
    Ok(Some(StoredHeader {
        state,
        retryable_reason: reason,
        generation,
        owner_nonce,
    }))
}

fn validate_optional_field(
    value: Option<&str>,
    maximum: usize,
) -> Result<(), SubmitIdempotencyError> {
    if value.is_some_and(|field| field.len() > maximum) {
        Err(SubmitIdempotencyError::ReceiptOversize)
    } else {
        Ok(())
    }
}

fn validate_receipt(
    binding: &SubmitIdempotencyBinding,
    receipt: &SubmitReceipt,
) -> Result<(), SubmitIdempotencyError> {
    validate_optional_field(receipt.agent_type.as_deref(), MAX_RECEIPT_FIELD_BYTES)?;
    validate_optional_field(receipt.profile_id.as_deref(), MAX_RECEIPT_FIELD_BYTES)?;
    validate_optional_field(receipt.profile_version.as_deref(), MAX_RECEIPT_FIELD_BYTES)?;
    validate_optional_field(receipt.cursor_before.as_deref(), MAX_RECEIPT_CURSOR_BYTES)?;
    validate_optional_field(receipt.cursor_after.as_deref(), MAX_RECEIPT_CURSOR_BYTES)?;
    if receipt.idempotency_key.len() > MAX_CALLER_KEY_BYTES
        || receipt.evidence_rule_ids.len() > MAX_RECEIPT_EVIDENCE_ITEMS
        || receipt
            .evidence_rule_ids
            .iter()
            .any(|item| item.len() > MAX_RECEIPT_FIELD_BYTES)
    {
        return Err(SubmitIdempotencyError::ReceiptOversize);
    }
    if receipt.idempotency_key.as_bytes() != binding.caller_key().as_bytes()
        || receipt.guarantee_met
            != receipt
                .guarantee_level
                .is_met_by(receipt.state, &receipt.evidence_rule_ids)
        || matches!(
            receipt.state,
            SubmitReceiptState::PolicyDenied
                | SubmitReceiptState::RequiresApproval
                | SubmitReceiptState::SendFailed
        )
    {
        return Err(SubmitIdempotencyError::ReceiptInvalid);
    }
    Ok(())
}

fn serialize_receipt(
    binding: &SubmitIdempotencyBinding,
    receipt: &SubmitReceipt,
) -> Result<String, SubmitIdempotencyError> {
    validate_receipt(binding, receipt)?;
    let envelope = StoredReceiptEnvelopeV1 {
        schema_version: RECEIPT_SCHEMA_VERSION,
        internal_claim_key: binding.key().to_string(),
        pane_id: binding.pane_id(),
        request_sha256: binding.request_sha256().to_string(),
        effect_sha256: binding.effect_sha256().to_string(),
        receipt: receipt.into(),
    };
    let json = serde_json::to_string(&envelope)
        .map_err(|_| SubmitIdempotencyError::ReceiptInvalid)?;
    if json.len() > MAX_RECEIPT_JSON_BYTES {
        return Err(SubmitIdempotencyError::ReceiptOversize);
    }
    Ok(json)
}

fn load_receipt(
    conn: &Connection,
    binding: &SubmitIdempotencyBinding,
) -> Result<SubmitReceipt, SubmitIdempotencyError> {
    let (length, bytes) = map_sqlite(
        conn
        .query_row(
            "SELECT length(CAST(receipt_json AS BLOB)), substr(CAST(receipt_json AS BLOB), 1, 65537) FROM verified_submit_idempotency WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY",
            [binding.key()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        ),
        SubmitIdempotencyError::RecordCorrupt,
    )?;
    if length < 0 || usize::try_from(length).map_or(true, |size| size > MAX_RECEIPT_JSON_BYTES) {
        return Err(SubmitIdempotencyError::ReceiptOversize);
    }
    if bytes.len() != usize::try_from(length).map_err(|_| SubmitIdempotencyError::RecordCorrupt)? {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    let json = std::str::from_utf8(&bytes).map_err(|_| SubmitIdempotencyError::ReceiptInvalid)?;
    let envelope: StoredReceiptEnvelopeV1 =
        serde_json::from_str(json).map_err(|_| SubmitIdempotencyError::ReceiptInvalid)?;
    if envelope.schema_version != RECEIPT_SCHEMA_VERSION
        || envelope.internal_claim_key.as_bytes() != binding.key().as_bytes()
        || envelope.pane_id != binding.pane_id()
        || envelope.request_sha256.as_bytes() != binding.request_sha256().as_bytes()
        || envelope.effect_sha256.as_bytes() != binding.effect_sha256().as_bytes()
    {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    let receipt = SubmitReceipt::from(envelope.receipt);
    validate_receipt(binding, &receipt).map_err(|error| match error {
        SubmitIdempotencyError::ReceiptOversize => error,
        _ => SubmitIdempotencyError::ReceiptInvalid,
    })?;
    Ok(receipt)
}

fn fresh_owner_nonce() -> Result<[u8; OWNER_NONCE_BYTES], SubmitIdempotencyError> {
    let mut nonce = [0_u8; OWNER_NONCE_BYTES];
    let mut rng = SysRng;
    rng.try_fill_bytes(&mut nonce)
        .map_err(|_| SubmitIdempotencyError::EntropyUnavailable)?;
    Ok(nonce)
}

fn store_usage(
    conn: &Connection,
    limits: StoreLimits,
) -> Result<(i64, i64), SubmitIdempotencyError> {
    map_sqlite(
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(?1 + length(CAST(idempotency_key AS BLOB)) + length(CAST(pane_id AS BLOB)) + length(CAST(request_sha256 AS BLOB)) + length(CAST(effect_sha256 AS BLOB)) + length(owner_nonce) + CASE WHEN state = ?2 THEN length(CAST(receipt_json AS BLOB)) WHEN state = ?3 THEN 0 ELSE ?4 END), 0) FROM verified_submit_idempotency",
            params![
                LOGICAL_RECORD_OVERHEAD_BYTES,
                STATE_COMPLETED,
                STATE_RETRYABLE,
                limits.receipt_reserve_bytes,
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ),
        SubmitIdempotencyError::ClaimFailed,
    )
}

fn new_record_logical_bytes(
    binding: &SubmitIdempotencyBinding,
    limits: StoreLimits,
) -> Result<i64, SubmitIdempotencyError> {
    let variable = binding
        .key()
        .len()
        .checked_add(binding.pane_id().to_string().len())
        .and_then(|value| value.checked_add(binding.request_sha256().len()))
        .and_then(|value| value.checked_add(binding.effect_sha256().len()))
        .and_then(|value| value.checked_add(OWNER_NONCE_BYTES))
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(SubmitIdempotencyError::CapacityExceeded)?;
    LOGICAL_RECORD_OVERHEAD_BYTES
        .checked_add(variable)
        .and_then(|value| value.checked_add(limits.receipt_reserve_bytes))
        .ok_or(SubmitIdempotencyError::CapacityExceeded)
}

fn ensure_new_record_capacity(
    conn: &Connection,
    binding: &SubmitIdempotencyBinding,
    limits: StoreLimits,
) -> Result<(), SubmitIdempotencyError> {
    let (records, bytes) = store_usage(conn, limits)?;
    let new_bytes = new_record_logical_bytes(binding, limits)?;
    if records >= limits.max_records
        || bytes
            .checked_add(new_bytes)
            .is_none_or(|total| total > limits.max_logical_bytes)
    {
        Err(SubmitIdempotencyError::CapacityExceeded)
    } else {
        Ok(())
    }
}

fn ensure_reclaim_capacity(
    conn: &Connection,
    limits: StoreLimits,
) -> Result<(), SubmitIdempotencyError> {
    let (_, bytes) = store_usage(conn, limits)?;
    if bytes
        .checked_add(limits.receipt_reserve_bytes)
        .is_none_or(|total| total > limits.max_logical_bytes)
    {
        Err(SubmitIdempotencyError::CapacityExceeded)
    } else {
        Ok(())
    }
}

/// Atomically claim a request before any possible injection side effect.
///
/// A `Retryable` row is atomically re-claimed. `InDoubt` never auto-retries;
/// callers must reconcile it explicitly. `Completed` returns the original,
/// bounded receipt.
///
/// # Errors
/// Returns a finite [`SubmitIdempotencyError`] for invalid bindings, unsafe
/// paths, unsupported/corrupt stores, or failed SQLite operations.
pub fn claim(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
) -> Result<ClaimOutcome, SubmitIdempotencyError> {
    claim_with_nonce_and_limits(ft_dir, binding, fresh_owner_nonce()?, PRODUCTION_LIMITS)
}

fn claim_with_nonce_and_limits(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    owner_nonce: [u8; OWNER_NONCE_BYTES],
    limits: StoreLimits,
) -> Result<ClaimOutcome, SubmitIdempotencyError> {
    validate_binding(binding)?;
    if limits.max_records < 1
        || limits.max_logical_bytes < 1
        || limits.receipt_reserve_bytes < 1
        || limits.receipt_reserve_bytes > MAX_RECEIPT_JSON_BYTES as i64
    {
        return Err(SubmitIdempotencyError::CapacityExceeded);
    }
    let mut conn = open_store(ft_dir, StoreOpenMode::Create)?
        .ok_or(SubmitIdempotencyError::OpenFailed)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::ClaimFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, true)?;
    match read_header(&tx, binding, SubmitIdempotencyError::ClaimFailed)? {
        None => {
            ensure_new_record_capacity(&tx, binding, limits)?;
            let now = now_unix_ms();
            let changed = map_sqlite(
                tx.execute(
                "INSERT INTO verified_submit_idempotency \
                 (idempotency_key, schema_version, pane_id, request_sha256, effect_sha256, state, \
                  retryable_reason, receipt_json, generation, owner_nonce, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, 1, ?7, ?8, ?8)",
                params![
                    binding.key(),
                    STORE_SCHEMA_VERSION,
                    binding.pane_id().to_string(),
                    binding.request_sha256(),
                    binding.effect_sha256(),
                    STATE_ACTIVE_OWNER,
                    &owner_nonce[..],
                    now,
                ],
                ),
                SubmitIdempotencyError::ClaimFailed,
            )?;
            if changed != 1 {
                return Err(SubmitIdempotencyError::ClaimFailed);
            }
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Claimed(ClaimToken {
                generation: 1,
                owner_nonce,
            }))
        }
        Some(header) if header.state == STATE_ACTIVE_OWNER => {
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::InFlight)
        }
        Some(header) if header.state == STATE_EFFECT_APPLIED_RECEIPT_PENDING => {
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::EffectAppliedReceiptPending)
        }
        Some(header) if header.state == STATE_IN_DOUBT => {
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::InDoubt)
        }
        Some(header) if header.state == STATE_COMPLETED => {
            let receipt = load_receipt(&tx, binding)?;
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Completed(receipt))
        }
        Some(header) if header.state == STATE_RETRYABLE => {
            ensure_reclaim_capacity(&tx, limits)?;
            let next_generation = header
                .generation
                .checked_add(1)
                .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
            let changed = map_sqlite(
                tx.execute(
                    "UPDATE verified_submit_idempotency \
                     SET state = ?2, retryable_reason = NULL, receipt_json = NULL, \
                         generation = ?3, owner_nonce = ?4, updated_unix_ms = ?5 \
                     WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?6 \
                       AND generation = ?7 AND owner_nonce = ?8",
                    params![
                        binding.key(),
                        STATE_ACTIVE_OWNER,
                        next_generation,
                        &owner_nonce[..],
                        now_unix_ms(),
                        STATE_RETRYABLE,
                        header.generation,
                        &header.owner_nonce[..],
                    ],
                ),
                SubmitIdempotencyError::ClaimFailed,
            )?;
            if changed != 1 {
                return Err(SubmitIdempotencyError::ClaimFailed);
            }
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Claimed(ClaimToken {
                generation: next_generation,
                owner_nonce,
            }))
        }
        Some(_) => Err(SubmitIdempotencyError::RecordCorrupt),
    }
}

/// Durably attach a bounded, fully bound receipt to an owned
/// `effect_applied_receipt_pending` claim.
/// Repeating the identical completion is idempotent; conflicting or retryable
/// transitions fail closed.
///
/// # Errors
/// Returns a finite [`SubmitIdempotencyError`] when the claim is absent,
/// malformed, not transitionable, too large, or cannot be committed.
pub fn complete(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    receipt: &SubmitReceipt,
) -> Result<(), SubmitIdempotencyError> {
    validate_binding(binding)?;
    let receipt_json = serialize_receipt(binding, receipt)?;
    let mut conn = open_store(ft_dir, StoreOpenMode::Existing)?
        .ok_or(SubmitIdempotencyError::MissingClaim)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let Some(header) = read_header(&tx, binding, SubmitIdempotencyError::TransitionFailed)? else {
        return Err(SubmitIdempotencyError::MissingClaim);
    };
    if header.generation != token.generation || header.owner_nonce != token.owner_nonce {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    if header.state == STATE_COMPLETED {
        let stored = load_receipt(&tx, binding)?;
        if stored != *receipt {
            return Err(SubmitIdempotencyError::InvalidTransition);
        }
        map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)?;
        return Ok(());
    }
    if header.state != STATE_EFFECT_APPLIED_RECEIPT_PENDING {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let changed = map_sqlite(
        tx.execute(
            "UPDATE verified_submit_idempotency \
             SET state = ?2, retryable_reason = NULL, receipt_json = ?3, updated_unix_ms = ?4 \
             WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?5 \
               AND generation = ?6 AND owner_nonce = ?7",
            params![
                binding.key(),
                STATE_COMPLETED,
                receipt_json,
                now_unix_ms(),
                STATE_EFFECT_APPLIED_RECEIPT_PENDING,
                token.generation,
                &token.owner_nonce[..],
            ],
        ),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    if changed != 1 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)
}

fn transition_from_active_owner(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    target_state: i64,
    retryable_reason: Option<RetryableReason>,
) -> Result<(), SubmitIdempotencyError> {
    validate_binding(binding)?;
    let mut conn = open_store(ft_dir, StoreOpenMode::Existing)?
        .ok_or(SubmitIdempotencyError::MissingClaim)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let Some(header) = read_header(&tx, binding, SubmitIdempotencyError::TransitionFailed)? else {
        return Err(SubmitIdempotencyError::MissingClaim);
    };
    if header.generation != token.generation || header.owner_nonce != token.owner_nonce {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let expected_reason = retryable_reason.map(RetryableReason::as_db);
    if header.state == target_state && header.retryable_reason == expected_reason {
        map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)?;
        return Ok(());
    }
    if header.state != STATE_ACTIVE_OWNER {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let changed = map_sqlite(
        tx.execute(
            "UPDATE verified_submit_idempotency SET state = ?2, retryable_reason = ?3, receipt_json = NULL, updated_unix_ms = ?4 WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?5 AND generation = ?6 AND owner_nonce = ?7",
            params![
                binding.key(),
                target_state,
                expected_reason,
                now_unix_ms(),
                STATE_ACTIVE_OWNER,
                token.generation,
                &token.owner_nonce[..],
            ],
        ),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    if changed != 1 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)
}

/// Record that the injector returned `Allowed` before any later observation,
/// audit enrichment, or receipt serialization is attempted.
///
/// # Errors
/// Returns a finite error when the token is stale or the durable transition
/// cannot be committed.
pub fn mark_effect_applied_receipt_pending(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
) -> Result<(), SubmitIdempotencyError> {
    transition_from_active_owner(
        ft_dir,
        binding,
        token,
        STATE_EFFECT_APPLIED_RECEIPT_PENDING,
        None,
    )
}

/// Record an injector/backend error whose pane-write outcome is ambiguous.
///
/// # Errors
/// Returns a finite error when the token is stale or the durable transition
/// cannot be committed.
pub fn mark_in_doubt(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
) -> Result<(), SubmitIdempotencyError> {
    transition_from_active_owner(ft_dir, binding, token, STATE_IN_DOUBT, None)
}

/// Mark a proven pre-effect denial as retryable. This transition must never be
/// used for an injector/backend error because that error may follow a write.
///
/// # Errors
/// Returns a finite [`SubmitIdempotencyError`] when the claim is absent,
/// malformed, not transitionable, or cannot be committed.
pub fn mark_retryable(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    reason: RetryableReason,
) -> Result<(), SubmitIdempotencyError> {
    transition_from_active_owner(ft_dir, binding, token, STATE_RETRYABLE, Some(reason))
}

/// Inspect a bound claim without changing its state.
///
/// # Errors
/// Returns a finite [`SubmitIdempotencyError`] when the binding/store/record is
/// invalid or the read transaction fails.
pub fn lookup(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
) -> Result<Option<StoredSubmitState>, SubmitIdempotencyError> {
    validate_binding(binding)?;
    let Some(mut conn) = open_store(ft_dir, StoreOpenMode::Existing)? else {
        return Ok(None);
    };
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::ClaimFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let state = match read_header(&tx, binding, SubmitIdempotencyError::ClaimFailed)? {
        None => None,
        Some(header) if header.state == STATE_ACTIVE_OWNER => Some(StoredSubmitState::ActiveOwner),
        Some(header) if header.state == STATE_EFFECT_APPLIED_RECEIPT_PENDING => {
            Some(StoredSubmitState::EffectAppliedReceiptPending)
        }
        Some(header) if header.state == STATE_IN_DOUBT => Some(StoredSubmitState::InDoubt),
        Some(header) if header.state == STATE_COMPLETED => {
            Some(StoredSubmitState::Completed(load_receipt(&tx, binding)?))
        }
        Some(header) if header.state == STATE_RETRYABLE => {
            let reason = header
                .retryable_reason
                .ok_or(SubmitIdempotencyError::RecordCorrupt)
                .and_then(RetryableReason::from_db)?;
            Some(StoredSubmitState::Retryable(reason))
        }
        Some(_) => return Err(SubmitIdempotencyError::RecordCorrupt),
    };
    map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::robot_types::SubmitReceiptState;
    use crate::verified_submit::idempotency_binding;
    use std::sync::{Arc, Barrier};

    fn binding(pane_id: u64, suffix: &str) -> SubmitIdempotencyBinding {
        idempotency_binding(pane_id, "deploy now", Some(suffix))
    }

    fn report(state: SubmitReceiptState) -> VerifiedSubmitReport {
        VerifiedSubmitReport {
            state,
            agent_type: Some("codex".to_string()),
            profile_id: Some("codex.default".to_string()),
            profile_version: Some("1".to_string()),
            attempts: 1,
            evidence_rule_ids: vec!["submit_profile:codex.default:submitted:0".to_string()],
            polls: 1,
            cursor_before: None,
            cursor_after: None,
        }
    }

    fn raw_connection(ft_dir: &Path) -> Connection {
        Connection::open(database_path(ft_dir)).expect("open test store")
    }

    fn claim_token(ft_dir: &Path, binding: &SubmitIdempotencyBinding) -> ClaimToken {
        match claim(ft_dir, binding).expect("claim") {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected a fresh claim, got {other:?}"),
        }
    }

    #[test]
    fn full_digest_keys_are_canonical_and_traversal_safe() {
        let generated = binding(42, "nonce");
        assert!(generated.is_canonical());
        assert!(is_valid_submit_key(generated.key()));
        assert_eq!(generated.request_sha256().len(), 64);
        assert_eq!(
            generated
                .key()
                .rsplit(':')
                .next()
                .expect("generated key digest")
                .len(),
            64
        );
        for invalid in [
            "../../../etc/passwd",
            "idem:7:../../../tmp/evil",
            "idem:7:0123456789abcdef",
            "idem:7:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "idem:07:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "idem:18446744073709551616:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(!is_valid_submit_key(invalid), "accepted {invalid:?}");
        }
    }

    #[test]
    fn claim_complete_and_reopen_replays_original_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(7, "reopen");
        let original = report(SubmitReceiptState::Submitted);
        let token = claim_token(dir.path(), &binding);
        complete(dir.path(), &binding, token, &original).expect("complete");
        assert_eq!(
            claim(dir.path(), &binding),
            Ok(ClaimOutcome::Completed(original.clone()))
        );
        assert_eq!(
            lookup(dir.path(), &binding),
            Ok(Some(StoredSubmitState::Completed(original)))
        );
    }

    #[test]
    fn absent_lookup_is_read_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(7, "absent");
        assert_eq!(lookup(dir.path(), &binding), Ok(None));
        assert!(!database_path(dir.path()).exists());
    }

    #[test]
    fn interrupted_owner_remains_in_doubt_after_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "crash");
        let _token = claim_token(dir.path(), &binding);
        assert_eq!(claim(dir.path(), &binding), Ok(ClaimOutcome::InDoubt));
        assert_eq!(
            lookup(dir.path(), &binding),
            Ok(Some(StoredSubmitState::InDoubt))
        );
    }

    #[test]
    fn concurrent_callers_have_exactly_one_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Arc::new(dir.path().to_path_buf());
        let binding = Arc::new(binding(9, "concurrent"));
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let binding = Arc::clone(&binding);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    claim(&path, &binding)
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("claim thread").expect("claim"))
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ClaimOutcome::Claimed(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ClaimOutcome::InDoubt))
                .count(),
            7
        );
    }

    #[test]
    fn proven_pre_effect_terminal_can_be_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(10, "denied");
        let first_token = claim_token(dir.path(), &binding);
        mark_retryable(
            dir.path(),
            &binding,
            first_token,
            RetryableReason::PolicyDenied,
        )
        .expect("mark retryable");
        assert_eq!(
            lookup(dir.path(), &binding),
            Ok(Some(StoredSubmitState::Retryable(
                RetryableReason::PolicyDenied
            )))
        );
        let _second_token = claim_token(dir.path(), &binding);
        assert_eq!(claim(dir.path(), &binding), Ok(ClaimOutcome::InDoubt));
    }

    #[test]
    fn stale_owner_cannot_transition_a_reclaimed_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(10, "aba");
        let first_token = claim_token(dir.path(), &binding);
        mark_retryable(
            dir.path(),
            &binding,
            first_token,
            RetryableReason::PolicyDenied,
        )
        .expect("release first generation");
        let second_token = claim_token(dir.path(), &binding);
        let submitted = report(SubmitReceiptState::Submitted);
        assert_eq!(
            complete(dir.path(), &binding, first_token, &submitted),
            Err(SubmitIdempotencyError::InvalidTransition)
        );
        assert_eq!(
            mark_retryable(
                dir.path(),
                &binding,
                first_token,
                RetryableReason::PolicyDenied,
            ),
            Err(SubmitIdempotencyError::InvalidTransition)
        );
        complete(dir.path(), &binding, second_token, &submitted)
            .expect("current owner completes");
    }

    #[test]
    fn proven_pre_effect_receipts_cannot_be_completed() {
        for (suffix, state) in [
            ("policy-denied", SubmitReceiptState::PolicyDenied),
            ("approval-required", SubmitReceiptState::RequiresApproval),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let binding = binding(10, suffix);
            let token = claim_token(dir.path(), &binding);
            assert_eq!(
                complete(dir.path(), &binding, token, &report(state)),
                Err(SubmitIdempotencyError::InvalidTransition)
            );
            assert_eq!(
                lookup(dir.path(), &binding),
                Ok(Some(StoredSubmitState::InDoubt)),
                "a rejected completion must not mutate the claim"
            );
        }
    }

    #[test]
    fn mismatched_pane_request_and_schema_fail_closed() {
        for (suffix, column, replacement) in [
            ("pane", "pane_id", "999"),
            (
                "request",
                "request_sha256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            ("schema", "schema_version", "999"),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let binding = binding(11, suffix);
            let _token = claim_token(dir.path(), &binding);
            let conn = raw_connection(dir.path());
            let sql = format!(
                "UPDATE verified_submit_idempotency SET {column} = ?1 WHERE idempotency_key = ?2"
            );
            conn.execute(&sql, params![replacement, binding.key()])
                .expect("corrupt binding");
            assert_eq!(
                lookup(dir.path(), &binding),
                Err(SubmitIdempotencyError::RecordCorrupt),
                "{column} mismatch must fail closed"
            );
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let original_binding = binding(11, "key");
        let replacement_key = binding(11, "different-request").key().to_string();
        let _token = claim_token(dir.path(), &original_binding);
        let conn = raw_connection(dir.path());
        conn.execute(
            "UPDATE verified_submit_idempotency SET idempotency_key = ?1 \
             WHERE idempotency_key = ?2",
            params![replacement_key, original_binding.key()],
        )
        .expect("corrupt key");
        assert_eq!(
            lookup(dir.path(), &original_binding),
            Err(SubmitIdempotencyError::RecordCorrupt),
            "key mismatch must fail closed through the independent request binding"
        );
    }

    #[test]
    fn malformed_and_oversize_completed_receipts_fail_before_allocation() {
        let invalid_dir = tempfile::tempdir().expect("tempdir");
        let invalid_binding = binding(12, "invalid-json");
        let _token = claim_token(invalid_dir.path(), &invalid_binding);
        let conn = raw_connection(invalid_dir.path());
        conn.execute(
            "UPDATE verified_submit_idempotency SET state = ?1, receipt_json = ?2 \
             WHERE idempotency_key = ?3",
            params![STATE_COMPLETED, "not-json", invalid_binding.key()],
        )
        .expect("inject invalid json");
        assert_eq!(
            lookup(invalid_dir.path(), &invalid_binding),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );

        let oversize_dir = tempfile::tempdir().expect("tempdir");
        let oversize_binding = binding(13, "oversize-db");
        let _token = claim_token(oversize_dir.path(), &oversize_binding);
        let conn = raw_connection(oversize_dir.path());
        conn.pragma_update(None, "ignore_check_constraints", true)
            .expect("disable checks for corruption fixture");
        conn.execute(
            "UPDATE verified_submit_idempotency SET state = ?1, receipt_json = ?2 \
             WHERE idempotency_key = ?3",
            params![
                STATE_COMPLETED,
                "x".repeat(MAX_RECEIPT_JSON_BYTES + 1),
                oversize_binding.key(),
            ],
        )
        .expect("inject oversize receipt");
        assert_eq!(
            lookup(oversize_dir.path(), &oversize_binding),
            Err(SubmitIdempotencyError::ReceiptOversize)
        );
    }

    #[test]
    fn completed_receipt_envelope_is_exact_and_fully_bound() {
        for field in [
            "schema_version",
            "idempotency_key",
            "pane_id",
            "request_sha256",
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let request_binding = binding(13, field);
            let token = claim_token(dir.path(), &request_binding);
            complete(
                dir.path(),
                &request_binding,
                token,
                &report(SubmitReceiptState::Submitted),
            )
            .expect("complete");
            let conn = raw_connection(dir.path());
            let json: String = conn
                .query_row(
                    "SELECT receipt_json FROM verified_submit_idempotency \
                     WHERE idempotency_key = ?1",
                    [request_binding.key()],
                    |row| row.get(0),
                )
                .expect("read envelope");
            let mut value: serde_json::Value =
                serde_json::from_str(&json).expect("parse envelope fixture");
            value[field] = match field {
                "schema_version" => serde_json::json!(STORE_SCHEMA_VERSION + 1),
                "idempotency_key" => serde_json::json!(binding(99, "other").key()),
                "pane_id" => serde_json::json!(99),
                "request_sha256" => serde_json::json!(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                _ => unreachable!(),
            };
            conn.execute(
                "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
                 WHERE idempotency_key = ?2",
                params![value.to_string(), request_binding.key()],
            )
            .expect("tamper envelope");
            assert_eq!(
                lookup(dir.path(), &request_binding),
                Err(SubmitIdempotencyError::RecordCorrupt),
                "envelope {field} mismatch must fail closed"
            );
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let request_binding = binding(13, "unknown-field");
        let token = claim_token(dir.path(), &request_binding);
        complete(
            dir.path(),
            &request_binding,
            token,
            &report(SubmitReceiptState::Submitted),
        )
        .expect("complete");
        let conn = raw_connection(dir.path());
        let json: String = conn
            .query_row(
                "SELECT receipt_json FROM verified_submit_idempotency \
                 WHERE idempotency_key = ?1",
                [request_binding.key()],
                |row| row.get(0),
            )
            .expect("read envelope");
        let mut value: serde_json::Value =
            serde_json::from_str(&json).expect("parse envelope fixture");
        value["unexpected"] = serde_json::json!(true);
        conn.execute(
            "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
             WHERE idempotency_key = ?2",
            params![value.to_string(), request_binding.key()],
        )
        .expect("add unknown field");
        assert_eq!(
            lookup(dir.path(), &request_binding),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );

        value
            .as_object_mut()
            .expect("receipt envelope object")
            .remove("unexpected");
        value["report"]["unexpected"] = serde_json::json!(true);
        conn.execute(
            "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
             WHERE idempotency_key = ?2",
            params![value.to_string(), request_binding.key()],
        )
        .expect("add unknown report field");
        assert_eq!(
            lookup(dir.path(), &request_binding),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );

        value["report"]
            .as_object_mut()
            .expect("stored report object")
            .remove("unexpected");
        value["report"]["state"] = serde_json::json!("policy_denied");
        conn.execute(
            "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
             WHERE idempotency_key = ?2",
            params![value.to_string(), request_binding.key()],
        )
        .expect("substitute pre-effect completed state");
        assert_eq!(
            lookup(dir.path(), &request_binding),
            Err(SubmitIdempotencyError::RecordCorrupt)
        );
    }

    #[test]
    fn initialized_database_never_recreates_a_missing_claim_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(13, "missing-table");
        let _token = claim_token(dir.path(), &binding);
        raw_connection(dir.path())
            .execute_batch("DROP TABLE verified_submit_idempotency")
            .expect("corrupt schema fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
    }

    #[test]
    fn initialized_database_requires_the_bound_request_unique_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(13, "missing-request-index");
        let _token = claim_token(dir.path(), &binding);
        raw_connection(dir.path())
            .execute_batch("DROP INDEX verified_submit_idempotency_request_unique")
            .expect("corrupt request uniqueness fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
        raw_connection(dir.path())
            .execute_batch(
                "CREATE UNIQUE INDEX verified_submit_idempotency_request_unique \
                 ON verified_submit_idempotency (request_sha256, pane_id)",
            )
            .expect("install reversed request uniqueness fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch),
            "the bound request index column order is part of schema v1"
        );
    }

    #[test]
    fn initialized_database_rejects_claim_side_effect_triggers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(13, "claim-trigger");
        let _token = claim_token(dir.path(), &binding);
        raw_connection(dir.path())
            .execute_batch(
                "CREATE TRIGGER delete_verified_submit_claim \
                 AFTER INSERT ON verified_submit_idempotency \
                 BEGIN \
                     DELETE FROM verified_submit_idempotency \
                     WHERE idempotency_key = NEW.idempotency_key; \
                 END;",
            )
            .expect("corrupt trigger fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
    }

    #[test]
    fn unsupported_database_schema_version_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(13, "schema-version");
        let _token = claim_token(dir.path(), &binding);
        raw_connection(dir.path())
            .pragma_update(None, "user_version", STORE_SCHEMA_VERSION + 1)
            .expect("corrupt schema version fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
    }

    #[test]
    fn incoming_oversize_report_leaves_conservative_in_doubt_claim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(14, "oversize-input");
        let token = claim_token(dir.path(), &binding);
        let mut oversized = report(SubmitReceiptState::Submitted);
        oversized.evidence_rule_ids = vec!["x".repeat(MAX_REPORT_FIELD_BYTES + 1)];
        assert_eq!(
            complete(dir.path(), &binding, token, &oversized),
            Err(SubmitIdempotencyError::ReceiptOversize)
        );
        assert_eq!(claim(dir.path(), &binding), Ok(ClaimOutcome::InDoubt));
    }

    #[cfg(unix)]
    #[test]
    fn database_symlink_is_rejected_without_following_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.sqlite3");
        std::fs::write(&target, b"must remain untouched").expect("write target");
        symlink(&target, database_path(dir.path())).expect("create symlink");
        let binding = binding(15, "symlink");
        assert_eq!(
            claim(dir.path(), &binding),
            Err(SubmitIdempotencyError::SymlinkRejected)
        );
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"must remain untouched"
        );
    }
}
