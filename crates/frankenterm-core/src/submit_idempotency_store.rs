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
use cap_fs_ext::{
    DirExt, FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt,
};
use cap_std::fs::{
    Dir as CapDir, File as CapFile, Metadata as CapMetadata, OpenOptions as CapOpenOptions,
};
#[cfg(unix)]
use cap_std::fs::{OpenOptionsExt as _, PermissionsExt as _};
use rand::{TryRng, rngs::SysRng};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STORE_FILENAME: &str = "submit_idempotency.sqlite3";
const STORE_ROLLBACK_JOURNAL_FILENAME: &str = "submit_idempotency.sqlite3-journal";
const STORE_WAL_FILENAME: &str = "submit_idempotency.sqlite3-wal";
const STORE_SHM_FILENAME: &str = "submit_idempotency.sqlite3-shm";
const STORE_AUXILIARY_FILENAMES: [&str; 3] = [
    STORE_ROLLBACK_JOURNAL_FILENAME,
    STORE_WAL_FILENAME,
    STORE_SHM_FILENAME,
];
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
// MCP routes these synchronous SQLite calls through its detached blocking-pool
// bridge. Keep lock acquisition fail-fast so contention cannot consume scarce
// blocking workers for the former five-second timeout; callers receive `Busy`.
const BUSY_TIMEOUT: Duration = Duration::from_millis(25);
const OWNER_NONCE_BYTES: usize = 32;
const OWNER_LEASE_DURATION_MS: i64 = 60_000;
const MAX_OWNER_LEASE_FUTURE_MS: i64 = OWNER_LEASE_DURATION_MS * 2;
const MAX_RECEIPT_JSON_BYTES: usize = 64 * 1024;
const MAX_RECEIPT_EVIDENCE_ITEMS: usize = 64;
const MAX_RECEIPT_FIELD_BYTES: usize = 1024;
const MAX_RECEIPT_CURSOR_BYTES: usize = 512;
const MAX_CALLER_KEY_BYTES: usize = 256;
const MAX_STORE_RECORDS: i64 = 16_384;
const MAX_STORE_LOGICAL_BYTES: i64 = 128 * 1024 * 1024;
const LOGICAL_RECORD_OVERHEAD_BYTES: i64 = 128;

const CREATE_TABLE_SQL: &str = "CREATE TABLE verified_submit_idempotency (idempotency_key TEXT COLLATE BINARY PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL, pane_id TEXT COLLATE BINARY NOT NULL, request_sha256 TEXT COLLATE BINARY NOT NULL, effect_sha256 TEXT COLLATE BINARY NOT NULL, state INTEGER NOT NULL CHECK (state IN (1, 2, 3, 4, 5)), retryable_reason INTEGER, receipt_json TEXT COLLATE BINARY, generation INTEGER NOT NULL CHECK (generation >= 1), owner_nonce BLOB NOT NULL CHECK (typeof(owner_nonce) = 'blob' AND length(owner_nonce) = 32), lease_expires_unix_ms INTEGER CHECK (lease_expires_unix_ms IS NULL OR lease_expires_unix_ms >= 0), created_unix_ms INTEGER NOT NULL, updated_unix_ms INTEGER NOT NULL, CHECK (created_unix_ms >= 0 AND updated_unix_ms >= created_unix_ms), CHECK (length(CAST(idempotency_key AS BLOB)) BETWEEN 71 AND 90), CHECK (length(CAST(pane_id AS BLOB)) BETWEEN 1 AND 20), CHECK (length(CAST(request_sha256 AS BLOB)) = 64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (length(CAST(effect_sha256 AS BLOB)) = 64 AND effect_sha256 NOT GLOB '*[^0-9a-f]*'), CHECK (receipt_json IS NULL OR length(CAST(receipt_json AS BLOB)) <= 65536), CHECK ((state = 1 AND retryable_reason IS NULL AND receipt_json IS NULL AND lease_expires_unix_ms IS NOT NULL) OR (state IN (2, 3) AND retryable_reason IS NULL AND receipt_json IS NULL AND lease_expires_unix_ms IS NULL) OR (state = 4 AND retryable_reason IS NULL AND receipt_json IS NOT NULL AND lease_expires_unix_ms IS NULL) OR (state = 5 AND retryable_reason IN (1, 2) AND receipt_json IS NULL AND lease_expires_unix_ms IS NULL))) STRICT, WITHOUT ROWID";
const CREATE_INDEX_SQL: &str = "CREATE INDEX verified_submit_idempotency_request_lookup ON verified_submit_idempotency (pane_id COLLATE BINARY, request_sha256 COLLATE BINARY)";

/// Finite failure taxonomy. No variant retains a filesystem path, SQL string,
/// serialized receipt, or backend error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitIdempotencyError {
    #[error("submit idempotency binding is invalid")]
    InvalidBinding,
    #[error("submit idempotency caller key is empty")]
    EmptyCallerKey,
    #[error("submit idempotency database path or sidecar is a symbolic link")]
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
    updated_unix_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct StoreLimits {
    max_records: i64,
    max_logical_bytes: i64,
    receipt_reserve_bytes: i64,
}

struct PreparedStorePath {
    database_path: PathBuf,
    directory_path: PathBuf,
    pinned_directory: CapDir,
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

fn validate_blank_schema(conn: &Connection) -> Result<(), SubmitIdempotencyError> {
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
    Ok(())
}

fn preflight_store_schema(
    conn: &Connection,
    allow_uninitialized: bool,
) -> Result<(), SubmitIdempotencyError> {
    match schema_header(conn)? {
        (STORE_APPLICATION_ID, STORE_SCHEMA_VERSION) => validate_initialized_schema_locked(conn),
        (0, 0) if allow_uninitialized => validate_blank_schema(conn),
        _ => Err(SubmitIdempotencyError::SchemaMismatch),
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
    validate_blank_schema(conn)?;
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

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, SubmitIdempotencyError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                normalized.push(std::path::MAIN_SEPARATOR_STR);
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(SubmitIdempotencyError::DirectoryUnavailable);
            }
            std::path::Component::Normal(name) => normalized.push(name),
        }
    }
    if normalized.as_os_str().is_empty() {
        Err(SubmitIdempotencyError::DirectoryUnavailable)
    } else {
        Ok(normalized)
    }
}

fn trusted_anchor_and_relative(
    directory: &Path,
) -> Result<(PathBuf, PathBuf), SubmitIdempotencyError> {
    // The platform temporary directory is an explicit ambient-authority anchor
    // for tests and ephemeral stores. On macOS it commonly traverses the
    // system-owned `/var` -> `/private/var` alias; components *below* the
    // trusted anchor are still opened one at a time without following links.
    let temp_anchor = normalized_absolute_path(&std::env::temp_dir())?;
    if directory.starts_with(&temp_anchor) {
        let relative = directory
            .strip_prefix(&temp_anchor)
            .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?
            .to_path_buf();
        return Ok((temp_anchor, relative));
    }
    let root = directory
        .ancestors()
        .last()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or(SubmitIdempotencyError::DirectoryUnavailable)?
        .to_path_buf();
    let relative = directory
        .strip_prefix(&root)
        .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?
        .to_path_buf();
    Ok((root, relative))
}

fn walk_store_directory_nofollow(
    ft_dir: &Path,
    mode: StoreOpenMode,
) -> Result<Option<(PathBuf, CapDir)>, SubmitIdempotencyError> {
    let directory = normalized_absolute_path(ft_dir)?;
    let (anchor, relative) = trusted_anchor_and_relative(&directory)?;
    let mut current = CapDir::open_ambient_dir(&anchor, cap_std::ambient_authority())
        .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(SubmitIdempotencyError::DirectoryUnavailable);
        };
        match current.symlink_metadata(name) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SubmitIdempotencyError::SymlinkRejected);
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(SubmitIdempotencyError::DirectoryUnavailable);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match mode {
                StoreOpenMode::Existing => return Ok(None),
                StoreOpenMode::Create => match current.create_dir(name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
                },
            },
            Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
        }
        current = current
            .open_dir_nofollow(name)
            .map_err(|_| SubmitIdempotencyError::SymlinkRejected)?;
    }
    Ok(Some((directory, current)))
}

#[cfg(unix)]
fn same_directory_identity(left: &CapDir, right: &CapDir) -> bool {
    let Ok(left) = left.dir_metadata() else {
        return false;
    };
    let Ok(right) = right.dir_metadata() else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_directory_identity(left: &CapDir, right: &CapDir) -> bool {
    left.dir_metadata().is_ok() && right.dir_metadata().is_ok()
}

#[cfg(unix)]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    left.is_file() && right.is_file()
}

fn store_leaf_metadata_nofollow(
    directory: &CapDir,
    filename: &str,
    required: bool,
) -> Result<Option<CapMetadata>, SubmitIdempotencyError> {
    match directory.symlink_metadata(filename) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(SubmitIdempotencyError::SymlinkRejected)
        }
        Ok(metadata) if !metadata.is_file() => Err(SubmitIdempotencyError::OpenFailed),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => Ok(None),
        Err(_) => Err(SubmitIdempotencyError::OpenFailed),
    }
}

fn harden_opened_store_leaf(
    directory: &CapDir,
    filename: &str,
    file: &CapFile,
) -> Result<(), SubmitIdempotencyError> {
    let opened_metadata = file
        .metadata()
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    if !opened_metadata.is_file() {
        return Err(SubmitIdempotencyError::OpenFailed);
    }
    #[cfg(unix)]
    if opened_metadata.permissions().mode() & 0o7777 != 0o600 {
        file.set_permissions(cap_std::fs::Permissions::from_mode(0o600))
            .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    }

    let named_metadata = store_leaf_metadata_nofollow(directory, filename, true)?
        .ok_or(SubmitIdempotencyError::OpenFailed)?;
    if same_file_identity(&opened_metadata, &named_metadata) {
        Ok(())
    } else {
        Err(SubmitIdempotencyError::SymlinkRejected)
    }
}

fn harden_existing_store_leaf(
    directory: &CapDir,
    filename: &str,
    required: bool,
) -> Result<bool, SubmitIdempotencyError> {
    let Some(before_open) = store_leaf_metadata_nofollow(directory, filename, required)? else {
        return Ok(false);
    };
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No);
    let file = directory
        .open_with(filename, &options)
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    if !same_file_identity(&before_open, &opened_metadata) {
        return Err(SubmitIdempotencyError::SymlinkRejected);
    }
    harden_opened_store_leaf(directory, filename, &file)?;
    Ok(true)
}

#[cfg(unix)]
fn sync_new_database_leaf(
    directory: &CapDir,
    file: &CapFile,
) -> Result<(), SubmitIdempotencyError> {
    file.sync_all()
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    directory
        .open(".")
        .and_then(|directory_file| directory_file.sync_all())
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)
}

#[cfg(not(unix))]
fn sync_new_database_leaf(
    _directory: &CapDir,
    _file: &CapFile,
) -> Result<(), SubmitIdempotencyError> {
    Ok(())
}

fn ensure_private_database_leaf(
    directory: &CapDir,
    mode: StoreOpenMode,
) -> Result<(), SubmitIdempotencyError> {
    if harden_existing_store_leaf(directory, STORE_FILENAME, false)? {
        return Ok(());
    }
    if matches!(mode, StoreOpenMode::Existing) {
        return Err(SubmitIdempotencyError::OpenFailed);
    }

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.mode(0o600);
    match directory.open_with(STORE_FILENAME, &options) {
        Ok(file) => {
            harden_opened_store_leaf(directory, STORE_FILENAME, &file)?;
            // SQLite now sees an existing empty file rather than performing
            // the creation itself, so persist both the leaf and its directory
            // entry before the first authority-bearing transaction.
            sync_new_database_leaf(directory, &file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if harden_existing_store_leaf(directory, STORE_FILENAME, true)? {
                Ok(())
            } else {
                Err(SubmitIdempotencyError::OpenFailed)
            }
        }
        Err(_) => Err(SubmitIdempotencyError::OpenFailed),
    }
}

fn harden_store_auxiliary_leaves(directory: &CapDir) -> Result<(), SubmitIdempotencyError> {
    for filename in STORE_AUXILIARY_FILENAMES {
        let _present = harden_existing_store_leaf(directory, filename, false)?;
    }
    Ok(())
}

fn prepare_store_path(
    ft_dir: &Path,
    mode: StoreOpenMode,
) -> Result<Option<PreparedStorePath>, SubmitIdempotencyError> {
    let Some((directory_path, pinned_directory)) =
        walk_store_directory_nofollow(ft_dir, mode)?
    else {
        return Ok(None);
    };

    match pinned_directory.symlink_metadata(LEGACY_STORE_NAME) {
        Ok(_) => return Err(SubmitIdempotencyError::LegacyStorePresent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
    }
    match pinned_directory.symlink_metadata(STORE_FILENAME) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SubmitIdempotencyError::SymlinkRejected);
        }
        Ok(metadata) if !metadata.is_file() => return Err(SubmitIdempotencyError::OpenFailed),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if matches!(mode, StoreOpenMode::Existing) {
                return Ok(None);
            }
        }
        Err(_) => return Err(SubmitIdempotencyError::OpenFailed),
    }
    for filename in STORE_AUXILIARY_FILENAMES {
        let _metadata = store_leaf_metadata_nofollow(&pinned_directory, filename, false)?;
    }
    Ok(Some(PreparedStorePath {
        database_path: database_path(&directory_path),
        directory_path,
        pinned_directory,
    }))
}

fn validate_connection_configuration(conn: &Connection) -> Result<(), SubmitIdempotencyError> {
    let journal_mode = map_sqlite(
        conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let synchronous = map_sqlite(
        conn.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let fullfsync = map_sqlite(
        conn.query_row("PRAGMA fullfsync", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let checkpoint_fullfsync = map_sqlite(
        conn.query_row("PRAGMA checkpoint_fullfsync", [], |row| {
            row.get::<_, i64>(0)
        }),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let trusted_schema = map_sqlite(
        conn.query_row("PRAGMA trusted_schema", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let foreign_keys = map_sqlite(
        conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    let busy_timeout_ms = map_sqlite(
        conn.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0)),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    if journal_mode.eq_ignore_ascii_case("wal")
        && synchronous == 2
        && fullfsync == 1
        && checkpoint_fullfsync == 1
        && trusted_schema == 0
        && foreign_keys == 1
        && busy_timeout_ms == i64::try_from(BUSY_TIMEOUT.as_millis()).unwrap_or(i64::MAX)
    {
        Ok(())
    } else {
        Err(SubmitIdempotencyError::ConfigurationFailed)
    }
}

fn open_store(
    ft_dir: &Path,
    mode: StoreOpenMode,
) -> Result<Option<Connection>, SubmitIdempotencyError> {
    let Some(prepared) = prepare_store_path(ft_dir, mode)? else {
        return Ok(None);
    };
    // Create the database leaf through the pinned capability with a private
    // mode before giving SQLite its path. Existing database and sidecar leaves
    // are opened without following the final component and re-hardened by file
    // handle, closing the usual permissive-umask window.
    ensure_private_database_leaf(&prepared.pinned_directory, mode)?;
    harden_store_auxiliary_leaves(&prepared.pinned_directory)?;
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if matches!(mode, StoreOpenMode::Create) {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let conn = Connection::open_with_flags(&prepared.database_path, flags)
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    let Some((_, current_directory)) =
        walk_store_directory_nofollow(&prepared.directory_path, StoreOpenMode::Existing)?
    else {
        return Err(SubmitIdempotencyError::DirectoryUnavailable);
    };
    if !same_directory_identity(&prepared.pinned_directory, &current_directory) {
        return Err(SubmitIdempotencyError::SymlinkRejected);
    }
    // SQLite accepts a path rather than an already-open capability, so a tiny
    // same-uid namespace race remains around its database/sidecar opens. We
    // pin and revalidate the directory, use NOFOLLOW for the database leaf,
    // reject and harden all known journal leaves before and after WAL setup,
    // and validate app/schema identity before WAL may mutate an existing DB.
    match current_directory.symlink_metadata(LEGACY_STORE_NAME) {
        Ok(_) => return Err(SubmitIdempotencyError::LegacyStorePresent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SubmitIdempotencyError::DirectoryUnavailable),
    }
    let _database_present =
        harden_existing_store_leaf(&current_directory, STORE_FILENAME, true)?;
    harden_store_auxiliary_leaves(&current_directory)?;
    map_sqlite(
        conn.busy_timeout(BUSY_TIMEOUT),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    for (pragma, value) in [
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
    // Reject a wrong application id, wrong version, non-empty blank-header DB,
    // or exact-schema spoof before `journal_mode=WAL` can write to it. The
    // authoritative validation still repeats under each IMMEDIATE transaction
    // to close races with another connection.
    preflight_store_schema(&conn, matches!(mode, StoreOpenMode::Create))?;
    map_sqlite(
        conn.pragma_update(None, "journal_mode", "WAL"),
        SubmitIdempotencyError::ConfigurationFailed,
    )?;
    harden_store_auxiliary_leaves(&current_directory)?;
    let Some((_, final_directory)) =
        walk_store_directory_nofollow(&prepared.directory_path, StoreOpenMode::Existing)?
    else {
        return Err(SubmitIdempotencyError::DirectoryUnavailable);
    };
    if !same_directory_identity(&prepared.pinned_directory, &final_directory) {
        return Err(SubmitIdempotencyError::SymlinkRejected);
    }
    validate_connection_configuration(&conn)?;
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
            "SELECT schema_version, length(CAST(idempotency_key AS BLOB)), substr(CAST(idempotency_key AS BLOB), 1, 91), length(CAST(pane_id AS BLOB)), substr(CAST(pane_id AS BLOB), 1, 21), length(CAST(request_sha256 AS BLOB)), substr(CAST(request_sha256 AS BLOB), 1, 65), length(CAST(effect_sha256 AS BLOB)), substr(CAST(effect_sha256 AS BLOB), 1, 65), state, retryable_reason, length(CAST(receipt_json AS BLOB)), generation, length(owner_nonce), substr(owner_nonce, 1, 33), lease_expires_unix_ms, created_unix_ms, updated_unix_ms FROM verified_submit_idempotency WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY",
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
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
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
        lease_expires_unix_ms,
        created_unix_ms,
        updated_unix_ms,
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
        || created_unix_ms < 0
        || updated_unix_ms < created_unix_ms
    {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    if request_digest != binding.request_sha256().as_bytes()
        || effect_digest != binding.effect_sha256().as_bytes()
    {
        return Err(SubmitIdempotencyError::RequestConflict);
    }
    let shape_ok = match state {
        STATE_ACTIVE_OWNER => {
            reason.is_none()
                && bytes.is_none()
                && lease_expires_unix_ms.is_some_and(|expires| expires >= 0)
        }
        STATE_EFFECT_APPLIED_RECEIPT_PENDING | STATE_IN_DOUBT => {
            reason.is_none() && bytes.is_none() && lease_expires_unix_ms.is_none()
        }
        STATE_COMPLETED => {
            reason.is_none()
                && lease_expires_unix_ms.is_none()
                && bytes.is_some_and(|value| {
                    value >= 0
                        && usize::try_from(value)
                            .is_ok_and(|size| size <= MAX_RECEIPT_JSON_BYTES)
                })
        }
        STATE_RETRYABLE => {
            bytes.is_none()
                && lease_expires_unix_ms.is_none()
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
        lease_expires_unix_ms,
        updated_unix_ms,
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
        || receipt.guarantee_level != binding.guarantee_level()
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

fn expire_active_owner_locked(
    conn: &Connection,
    binding: &SubmitIdempotencyBinding,
    header: StoredHeader,
    now: i64,
    fallback: SubmitIdempotencyError,
) -> Result<(), SubmitIdempotencyError> {
    let lease_expires = header
        .lease_expires_unix_ms
        .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
    let persisted_update = now.max(header.updated_unix_ms);
    let changed = map_sqlite(
        conn.execute(
            "UPDATE verified_submit_idempotency SET state = ?2, lease_expires_unix_ms = NULL, updated_unix_ms = ?3 WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?4 AND generation = ?5 AND owner_nonce = ?6 AND lease_expires_unix_ms = ?7",
            params![
                binding.key(),
                STATE_IN_DOUBT,
                persisted_update,
                STATE_ACTIVE_OWNER,
                header.generation,
                &header.owner_nonce[..],
                lease_expires,
            ],
        ),
        fallback,
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(fallback)
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
    claim_with_nonce_factory_limits_and_clock(
        ft_dir,
        binding,
        fresh_owner_nonce,
        PRODUCTION_LIMITS,
        now_unix_ms,
    )
}

#[cfg(test)]
fn claim_with_nonce_limits_and_time(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    owner_nonce: [u8; OWNER_NONCE_BYTES],
    limits: StoreLimits,
    now: i64,
) -> Result<ClaimOutcome, SubmitIdempotencyError> {
    claim_with_nonce_limits_and_clock(ft_dir, binding, owner_nonce, limits, move || now)
}

#[cfg(test)]
fn claim_with_nonce_limits_and_clock<F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    owner_nonce: [u8; OWNER_NONCE_BYTES],
    limits: StoreLimits,
    clock: F,
) -> Result<ClaimOutcome, SubmitIdempotencyError>
where
    F: FnMut() -> i64,
{
    claim_with_nonce_factory_limits_and_clock(
        ft_dir,
        binding,
        move || Ok(owner_nonce),
        limits,
        clock,
    )
}

fn claim_with_nonce_factory_limits_and_clock<N, F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    mut owner_nonce_factory: N,
    limits: StoreLimits,
    mut clock: F,
) -> Result<ClaimOutcome, SubmitIdempotencyError>
where
    N: FnMut() -> Result<[u8; OWNER_NONCE_BYTES], SubmitIdempotencyError>,
    F: FnMut() -> i64,
{
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
    // Stamp the lease only after path opening, PRAGMA setup, schema validation,
    // and IMMEDIATE-transaction acquisition. A slow prelude therefore cannot
    // consume the owner's lease before the authority-bearing transaction even
    // begins.
    let now = clock();
    if now < 0 {
        return Err(SubmitIdempotencyError::ClaimFailed);
    }
    match read_header(&tx, binding, SubmitIdempotencyError::ClaimFailed)? {
        None => {
            ensure_new_record_capacity(&tx, binding, limits)?;
            let owner_nonce = owner_nonce_factory()?;
            let lease_expires = now
                .checked_add(OWNER_LEASE_DURATION_MS)
                .ok_or(SubmitIdempotencyError::ClaimFailed)?;
            let changed = map_sqlite(
                tx.execute(
                "INSERT INTO verified_submit_idempotency \
                 (idempotency_key, schema_version, pane_id, request_sha256, effect_sha256, state, \
                  retryable_reason, receipt_json, generation, owner_nonce, lease_expires_unix_ms, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, 1, ?7, ?8, ?9, ?9)",
                params![
                    binding.key(),
                    STORE_SCHEMA_VERSION,
                    binding.pane_id().to_string(),
                    binding.request_sha256(),
                    binding.effect_sha256(),
                    STATE_ACTIVE_OWNER,
                    &owner_nonce[..],
                    lease_expires,
                    now,
                ],
                ),
                SubmitIdempotencyError::ClaimFailed,
            )?;
            if changed != 1 {
                return Err(SubmitIdempotencyError::ClaimFailed);
            }
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            claim_outcome_after_commit(
                ft_dir,
                binding,
                ClaimToken {
                    generation: 1,
                    owner_nonce,
                },
                lease_expires,
                clock(),
            )
        }
        Some(header) if header.state == STATE_ACTIVE_OWNER => {
            let lease_expires = header
                .lease_expires_unix_ms
                .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
            let maximum_credible_expiry = now
                .checked_add(MAX_OWNER_LEASE_FUTURE_MS)
                .unwrap_or(i64::MAX);
            if lease_expires > now && lease_expires <= maximum_credible_expiry {
                map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
                return Ok(ClaimOutcome::InFlight);
            }
            expire_active_owner_locked(
                &tx,
                binding,
                header,
                now,
                SubmitIdempotencyError::ClaimFailed,
            )?;
            map_sqlite(tx.commit(), SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::InDoubt)
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
            let owner_nonce = owner_nonce_factory()?;
            let next_generation = header
                .generation
                .checked_add(1)
                .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
            let lease_expires = now
                .checked_add(OWNER_LEASE_DURATION_MS)
                .ok_or(SubmitIdempotencyError::ClaimFailed)?;
            let persisted_update = now.max(header.updated_unix_ms);
            let changed = map_sqlite(
                tx.execute(
                    "UPDATE verified_submit_idempotency \
                     SET state = ?2, retryable_reason = NULL, receipt_json = NULL, \
                         generation = ?3, owner_nonce = ?4, lease_expires_unix_ms = ?5, updated_unix_ms = ?6 \
                     WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?7 \
                       AND generation = ?8 AND owner_nonce = ?9",
                    params![
                        binding.key(),
                        STATE_ACTIVE_OWNER,
                        next_generation,
                        &owner_nonce[..],
                        lease_expires,
                        persisted_update,
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
            claim_outcome_after_commit(
                ft_dir,
                binding,
                ClaimToken {
                    generation: next_generation,
                    owner_nonce,
                },
                lease_expires,
                clock(),
            )
        }
        Some(_) => Err(SubmitIdempotencyError::RecordCorrupt),
    }
}

fn claim_outcome_after_commit(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    lease_expires: i64,
    returned_at: i64,
) -> Result<ClaimOutcome, SubmitIdempotencyError> {
    if returned_at < 0 {
        return Err(SubmitIdempotencyError::ClaimFailed);
    }
    if returned_at < lease_expires {
        return Ok(ClaimOutcome::Claimed(token));
    }
    match lookup_at(ft_dir, binding, returned_at)? {
        Some(StoredSubmitState::InDoubt) => Ok(ClaimOutcome::InDoubt),
        _ => Err(SubmitIdempotencyError::ClaimFailed),
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
    complete_with_clock(ft_dir, binding, token, receipt, now_unix_ms)
}

#[cfg(test)]
fn complete_at(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    receipt: &SubmitReceipt,
    completed_at: i64,
) -> Result<(), SubmitIdempotencyError> {
    complete_with_clock(ft_dir, binding, token, receipt, move || completed_at)
}

fn complete_with_clock<F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    receipt: &SubmitReceipt,
    mut clock: F,
) -> Result<(), SubmitIdempotencyError>
where
    F: FnMut() -> i64,
{
    validate_binding(binding)?;
    let receipt_json = serialize_receipt(binding, receipt)?;
    let mut conn = open_store(ft_dir, StoreOpenMode::Existing)?
        .ok_or(SubmitIdempotencyError::MissingClaim)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let completed_at = clock();
    if completed_at < 0 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
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
    let persisted_update = completed_at.max(header.updated_unix_ms);
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
                persisted_update,
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
    transition_from_active_owner_with_clock(
        ft_dir,
        binding,
        token,
        target_state,
        retryable_reason,
        now_unix_ms,
    )
}

#[cfg(test)]
fn transition_from_active_owner_at(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    target_state: i64,
    retryable_reason: Option<RetryableReason>,
    now: i64,
) -> Result<(), SubmitIdempotencyError> {
    transition_from_active_owner_with_clock(
        ft_dir,
        binding,
        token,
        target_state,
        retryable_reason,
        move || now,
    )
}

fn transition_from_active_owner_with_clock<F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    target_state: i64,
    retryable_reason: Option<RetryableReason>,
    mut clock: F,
) -> Result<(), SubmitIdempotencyError>
where
    F: FnMut() -> i64,
{
    validate_binding(binding)?;
    let mut conn = open_store(ft_dir, StoreOpenMode::Existing)?
        .ok_or(SubmitIdempotencyError::MissingClaim)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let now = clock();
    if now < 0 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
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
    let lease_expires = header
        .lease_expires_unix_ms
        .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
    let maximum_credible_expiry = now
        .checked_add(MAX_OWNER_LEASE_FUTURE_MS)
        .unwrap_or(i64::MAX);
    if lease_expires <= now || lease_expires > maximum_credible_expiry {
        expire_active_owner_locked(
            &tx,
            binding,
            header,
            now,
            SubmitIdempotencyError::TransitionFailed,
        )?;
        map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)?;
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let persisted_update = now.max(header.updated_unix_ms);
    let changed = map_sqlite(
        tx.execute(
            "UPDATE verified_submit_idempotency SET state = ?2, retryable_reason = ?3, receipt_json = NULL, lease_expires_unix_ms = NULL, updated_unix_ms = ?4 WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?5 AND generation = ?6 AND owner_nonce = ?7 AND lease_expires_unix_ms = ?8",
            params![
                binding.key(),
                target_state,
                expected_reason,
                persisted_update,
                STATE_ACTIVE_OWNER,
                token.generation,
                &token.owner_nonce[..],
                lease_expires,
            ],
        ),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    if changed != 1 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)
}

/// Renew a live owner lease without changing its fencing generation or nonce.
/// An already-expired or implausibly future lease is fenced to `in_doubt` and
/// cannot be revived.
///
/// # Errors
/// Returns a finite error when the claim is stale, expired, malformed, or the
/// renewal cannot be committed.
pub fn renew_claim(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
) -> Result<(), SubmitIdempotencyError> {
    renew_claim_with_clock(ft_dir, binding, token, now_unix_ms)
}

#[cfg(test)]
fn renew_claim_at(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    now: i64,
) -> Result<(), SubmitIdempotencyError> {
    renew_claim_with_clock(ft_dir, binding, token, move || now)
}

fn renew_claim_with_clock<F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    token: ClaimToken,
    mut clock: F,
) -> Result<(), SubmitIdempotencyError>
where
    F: FnMut() -> i64,
{
    validate_binding(binding)?;
    let mut conn = open_store(ft_dir, StoreOpenMode::Existing)?
        .ok_or(SubmitIdempotencyError::MissingClaim)?;
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::TransitionFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let now = clock();
    if now < 0 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    let Some(header) = read_header(&tx, binding, SubmitIdempotencyError::TransitionFailed)? else {
        return Err(SubmitIdempotencyError::MissingClaim);
    };
    if header.generation != token.generation || header.owner_nonce != token.owner_nonce {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    if header.state != STATE_ACTIVE_OWNER {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let old_expiry = header
        .lease_expires_unix_ms
        .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
    let maximum_credible_expiry = now
        .checked_add(MAX_OWNER_LEASE_FUTURE_MS)
        .unwrap_or(i64::MAX);
    if old_expiry <= now || old_expiry > maximum_credible_expiry {
        expire_active_owner_locked(
            &tx,
            binding,
            header,
            now,
            SubmitIdempotencyError::TransitionFailed,
        )?;
        map_sqlite(tx.commit(), SubmitIdempotencyError::TransitionFailed)?;
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let new_expiry = now
        .checked_add(OWNER_LEASE_DURATION_MS)
        .ok_or(SubmitIdempotencyError::TransitionFailed)?
        .max(old_expiry);
    let persisted_update = now.max(header.updated_unix_ms);
    let changed = map_sqlite(
        tx.execute(
            "UPDATE verified_submit_idempotency SET lease_expires_unix_ms = ?2, updated_unix_ms = ?3 WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY AND state = ?4 AND generation = ?5 AND owner_nonce = ?6 AND lease_expires_unix_ms = ?7",
            params![
                binding.key(),
                new_expiry,
                persisted_update,
                STATE_ACTIVE_OWNER,
                token.generation,
                &token.owner_nonce[..],
                old_expiry,
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
    lookup_with_clock(ft_dir, binding, now_unix_ms)
}

fn lookup_at(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    now: i64,
) -> Result<Option<StoredSubmitState>, SubmitIdempotencyError> {
    lookup_with_clock(ft_dir, binding, move || now)
}

fn lookup_with_clock<F>(
    ft_dir: &Path,
    binding: &SubmitIdempotencyBinding,
    mut clock: F,
) -> Result<Option<StoredSubmitState>, SubmitIdempotencyError>
where
    F: FnMut() -> i64,
{
    validate_binding(binding)?;
    let Some(mut conn) = open_store(ft_dir, StoreOpenMode::Existing)? else {
        return Ok(None);
    };
    let tx = map_sqlite(
        conn.transaction_with_behavior(TransactionBehavior::Immediate),
        SubmitIdempotencyError::ClaimFailed,
    )?;
    initialize_or_validate_schema_locked(&tx, false)?;
    let now = clock();
    if now < 0 {
        return Err(SubmitIdempotencyError::ClaimFailed);
    }
    let state = match read_header(&tx, binding, SubmitIdempotencyError::ClaimFailed)? {
        None => None,
        Some(header) if header.state == STATE_ACTIVE_OWNER => {
            let lease_expires = header
                .lease_expires_unix_ms
                .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
            let maximum_credible_expiry = now
                .checked_add(MAX_OWNER_LEASE_FUTURE_MS)
                .unwrap_or(i64::MAX);
            if lease_expires > now && lease_expires <= maximum_credible_expiry {
                Some(StoredSubmitState::ActiveOwner)
            } else {
                expire_active_owner_locked(
                    &tx,
                    binding,
                    header,
                    now,
                    SubmitIdempotencyError::ClaimFailed,
                )?;
                Some(StoredSubmitState::InDoubt)
            }
        }
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
    use crate::robot_types::{SubmitGuaranteeLevel, SubmitReceiptState};
    use crate::verified_submit::{SubmitIdempotencyRequest, idempotency_binding};
    use std::sync::{Arc, Barrier};

    fn binding(pane_id: u64, suffix: &str) -> SubmitIdempotencyBinding {
        idempotency_binding(SubmitIdempotencyRequest {
            pane_id,
            text: "deploy now",
            caller_key: suffix,
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            wait_for: None,
            wait_for_regex: false,
            timeout_secs: 30,
        })
    }

    fn receipt(binding: &SubmitIdempotencyBinding, state: SubmitReceiptState) -> SubmitReceipt {
        let evidence_rule_ids = vec!["submit_profile:codex.default:submitted:0".to_string()];
        SubmitReceipt {
            state,
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            guarantee_met: SubmitGuaranteeLevel::Submitted.is_met_by(state, &evidence_rule_ids),
            agent_type: Some("codex".to_string()),
            profile_id: Some("codex.default".to_string()),
            profile_version: Some("1".to_string()),
            attempts: 1,
            evidence_rule_ids,
            elapsed_ms: 42,
            polls: 1,
            cursor_before: None,
            cursor_after: None,
            idempotency_key: binding.caller_key().to_string(),
        }
    }

    fn complete_owned(
        ft_dir: &Path,
        binding: &SubmitIdempotencyBinding,
        token: ClaimToken,
        receipt: &SubmitReceipt,
    ) {
        mark_effect_applied_receipt_pending(ft_dir, binding, token).expect("mark effect applied");
        complete(ft_dir, binding, token, receipt).expect("complete receipt");
    }

    fn raw_connection(ft_dir: &Path) -> Connection {
        Connection::open(database_path(ft_dir)).expect("open test store")
    }

    fn persisted_updated_at(ft_dir: &Path, binding: &SubmitIdempotencyBinding) -> i64 {
        raw_connection(ft_dir)
            .query_row(
                "SELECT updated_unix_ms FROM verified_submit_idempotency WHERE idempotency_key = ?1",
                [binding.key()],
                |row| row.get(0),
            )
            .expect("read persisted update timestamp")
    }

    fn claim_token(ft_dir: &Path, binding: &SubmitIdempotencyBinding) -> ClaimToken {
        match claim(ft_dir, binding).expect("claim") {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected a fresh claim, got {other:?}"),
        }
    }

    fn claim_without_entropy_at(
        ft_dir: &Path,
        binding: &SubmitIdempotencyBinding,
        now: i64,
    ) -> Result<ClaimOutcome, SubmitIdempotencyError> {
        claim_with_nonce_factory_limits_and_clock(
            ft_dir,
            binding,
            || Err(SubmitIdempotencyError::EntropyUnavailable),
            PRODUCTION_LIMITS,
            move || now,
        )
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

        let max_pane = binding(u64::MAX, "max-pane");
        assert_eq!(max_pane.key().len(), 90);
        assert!(is_valid_submit_key(max_pane.key()));
        let dir = tempfile::tempdir().expect("tempdir");
        let _token = claim_token(dir.path(), &max_pane);
        assert_eq!(
            lookup(dir.path(), &max_pane),
            Ok(Some(StoredSubmitState::ActiveOwner))
        );
    }

    #[test]
    fn claim_complete_and_reopen_replays_original_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(7, "reopen");
        let original = receipt(&binding, SubmitReceiptState::Submitted);
        let token = claim_token(dir.path(), &binding);
        complete_owned(dir.path(), &binding, token, &original);
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
    fn only_claim_outcomes_that_acquire_ownership_require_fresh_entropy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = now_unix_ms();

        let active = binding(7, "entropy-active");
        let _active_token = claim_with_nonce_limits_and_time(
            dir.path(),
            &active,
            [1; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            now,
        )
        .expect("create active owner");
        assert_eq!(
            claim_without_entropy_at(dir.path(), &active, now + 1),
            Ok(ClaimOutcome::InFlight)
        );

        let pending = binding(7, "entropy-pending");
        let pending_token = claim_token(dir.path(), &pending);
        mark_effect_applied_receipt_pending(dir.path(), &pending, pending_token)
            .expect("mark pending");
        assert_eq!(
            claim_without_entropy_at(dir.path(), &pending, now + 1),
            Ok(ClaimOutcome::EffectAppliedReceiptPending)
        );

        let in_doubt = binding(7, "entropy-in-doubt");
        let in_doubt_token = claim_token(dir.path(), &in_doubt);
        mark_in_doubt(dir.path(), &in_doubt, in_doubt_token).expect("mark in doubt");
        assert_eq!(
            claim_without_entropy_at(dir.path(), &in_doubt, now + 1),
            Ok(ClaimOutcome::InDoubt)
        );

        let completed = binding(7, "entropy-completed");
        let completed_receipt = receipt(&completed, SubmitReceiptState::Submitted);
        let completed_token = claim_token(dir.path(), &completed);
        complete_owned(
            dir.path(),
            &completed,
            completed_token,
            &completed_receipt,
        );
        assert_eq!(
            claim_without_entropy_at(dir.path(), &completed, now + 1),
            Ok(ClaimOutcome::Completed(completed_receipt))
        );

        let retryable = binding(7, "entropy-retryable");
        let retryable_token = claim_token(dir.path(), &retryable);
        mark_retryable(
            dir.path(),
            &retryable,
            retryable_token,
            RetryableReason::PolicyDenied,
        )
        .expect("mark retryable");
        assert_eq!(
            claim_without_entropy_at(dir.path(), &retryable, now + 1),
            Err(SubmitIdempotencyError::EntropyUnavailable),
            "reclaiming Retryable must still fail closed without fresh owner entropy"
        );

        let absent = binding(7, "entropy-absent");
        assert_eq!(
            claim_without_entropy_at(dir.path(), &absent, now + 1),
            Err(SubmitIdempotencyError::EntropyUnavailable),
            "creating a new owner must still fail closed without fresh entropy"
        );
    }

    #[test]
    fn effect_pending_and_ambiguous_error_are_distinct_terminal_barriers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pending = binding(7, "effect-pending");
        let pending_token = claim_token(dir.path(), &pending);
        mark_effect_applied_receipt_pending(dir.path(), &pending, pending_token)
            .expect("mark pending");
        assert_eq!(
            claim(dir.path(), &pending),
            Ok(ClaimOutcome::EffectAppliedReceiptPending)
        );

        let ambiguous = binding(7, "ambiguous-error");
        let ambiguous_token = claim_token(dir.path(), &ambiguous);
        mark_in_doubt(dir.path(), &ambiguous, ambiguous_token).expect("mark in doubt");
        assert_eq!(
            claim(dir.path(), &ambiguous),
            Ok(ClaimOutcome::InDoubt)
        );
    }

    #[test]
    fn storage_local_v1_enums_convert_every_public_variant_exhaustively() {
        for state in [
            SubmitReceiptState::Submitted,
            SubmitReceiptState::QueuedBehindOperation,
            SubmitReceiptState::StuckInComposer,
            SubmitReceiptState::PaneCrashedToShell,
            SubmitReceiptState::VerificationUnavailable,
            SubmitReceiptState::PolicyDenied,
            SubmitReceiptState::RequiresApproval,
            SubmitReceiptState::SendFailed,
        ] {
            assert_eq!(
                SubmitReceiptState::from(StoredSubmitReceiptStateV1::from(state)),
                state
            );
        }
        for level in [
            SubmitGuaranteeLevel::Write,
            SubmitGuaranteeLevel::Composer,
            SubmitGuaranteeLevel::Submitted,
            SubmitGuaranteeLevel::Working,
        ] {
            assert_eq!(
                SubmitGuaranteeLevel::from(StoredSubmitGuaranteeLevelV1::from(level)),
                level
            );
        }
    }

    #[test]
    fn guarantee_met_and_caller_key_are_recomputed_before_completion() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(7, "receipt-authority");
        let token = claim_token(dir.path(), &binding);
        mark_effect_applied_receipt_pending(dir.path(), &binding, token).expect("mark pending");
        let mut invalid = receipt(&binding, SubmitReceiptState::Submitted);
        invalid.guarantee_met = false;
        assert_eq!(
            complete(dir.path(), &binding, token, &invalid),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );
        invalid.guarantee_met = true;
        invalid.guarantee_level = SubmitGuaranteeLevel::Working;
        invalid.guarantee_met = false;
        assert_eq!(
            complete(dir.path(), &binding, token, &invalid),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );
        invalid.guarantee_level = binding.guarantee_level();
        invalid.guarantee_met = true;
        invalid.idempotency_key = "different-caller-key".to_string();
        assert_eq!(
            complete(dir.path(), &binding, token, &invalid),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );
        assert_eq!(
            lookup(dir.path(), &binding),
            Ok(Some(StoredSubmitState::EffectAppliedReceiptPending))
        );
    }

    #[test]
    fn caller_nonce_reuse_with_changed_semantics_is_a_conflict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = binding(7, "same-caller-key");
        let _token = claim_token(dir.path(), &original);
        let changed_text = idempotency_binding(SubmitIdempotencyRequest {
            pane_id: 7,
            text: "deploy later",
            caller_key: "same-caller-key",
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            wait_for: None,
            wait_for_regex: false,
            timeout_secs: 30,
        });
        let changed_wait = idempotency_binding(SubmitIdempotencyRequest {
            pane_id: 7,
            text: "deploy now",
            caller_key: "same-caller-key",
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            wait_for: Some("ready"),
            wait_for_regex: true,
            timeout_secs: 31,
        });
        assert_eq!(original.key(), changed_text.key());
        assert_eq!(original.key(), changed_wait.key());
        assert_eq!(
            claim(dir.path(), &changed_text),
            Err(SubmitIdempotencyError::RequestConflict)
        );
        assert_eq!(
            lookup(dir.path(), &changed_wait),
            Err(SubmitIdempotencyError::RequestConflict)
        );
    }

    #[test]
    fn distinct_caller_nonces_can_authorize_identical_effects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = binding(7, "authorized-attempt-1");
        let second = binding(7, "authorized-attempt-2");
        assert_ne!(first.key(), second.key());
        assert_eq!(first.request_sha256(), second.request_sha256());
        assert_eq!(first.effect_sha256(), second.effect_sha256());
        let _first = claim_token(dir.path(), &first);
        let _second = claim_token(dir.path(), &second);
        assert_eq!(
            lookup(dir.path(), &first),
            Ok(Some(StoredSubmitState::ActiveOwner))
        );
        assert_eq!(
            lookup(dir.path(), &second),
            Ok(Some(StoredSubmitState::ActiveOwner))
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
    fn live_owner_is_in_flight_then_expiry_fences_it_in_doubt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "crash");
        let started_at = 1_000_000;
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [1; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            started_at,
        )
        .expect("initial claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        assert_eq!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &binding,
                [2; OWNER_NONCE_BYTES],
                PRODUCTION_LIMITS,
                started_at + 1,
            ),
            Ok(ClaimOutcome::InFlight)
        );
        assert_eq!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &binding,
                [3; OWNER_NONCE_BYTES],
                PRODUCTION_LIMITS,
                started_at + OWNER_LEASE_DURATION_MS,
            ),
            Ok(ClaimOutcome::InDoubt)
        );
        assert_eq!(
            lookup_at(
                dir.path(),
                &binding,
                started_at + OWNER_LEASE_DURATION_MS + 1
            ),
            Ok(Some(StoredSubmitState::InDoubt))
        );
        assert_eq!(
            renew_claim_at(
                dir.path(),
                &binding,
                token,
                started_at + OWNER_LEASE_DURATION_MS + 1,
            ),
            Err(SubmitIdempotencyError::InvalidTransition)
        );
    }

    #[test]
    fn renewal_fences_stale_owner_before_effect_and_expiry_during_effect() {
        let before_dir = tempfile::tempdir().expect("tempdir");
        let before_binding = binding(8, "stale-before-effect");
        let started_at = 20_000;
        let before_token = match claim_with_nonce_limits_and_time(
            before_dir.path(),
            &before_binding,
            [4; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            started_at,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        assert_eq!(
            renew_claim_at(
                before_dir.path(),
                &before_binding,
                before_token,
                started_at + OWNER_LEASE_DURATION_MS,
            ),
            Err(SubmitIdempotencyError::InvalidTransition),
            "a stale owner must fail its immediately-pre-effect fence"
        );
        assert_eq!(
            lookup_at(
                before_dir.path(),
                &before_binding,
                started_at + OWNER_LEASE_DURATION_MS,
            ),
            Ok(Some(StoredSubmitState::InDoubt))
        );

        let during_dir = tempfile::tempdir().expect("tempdir");
        let during_binding = binding(8, "expiry-during-effect");
        let during_token = match claim_with_nonce_limits_and_time(
            during_dir.path(),
            &during_binding,
            [5; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            started_at,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        renew_claim_at(
            during_dir.path(),
            &during_binding,
            during_token,
            started_at + OWNER_LEASE_DURATION_MS - 1,
        )
        .expect("pre-effect fence renewal");
        assert_eq!(
            transition_from_active_owner_at(
                during_dir.path(),
                &during_binding,
                during_token,
                STATE_EFFECT_APPLIED_RECEIPT_PENDING,
                None,
                started_at + (OWNER_LEASE_DURATION_MS * 2),
            ),
            Err(SubmitIdempotencyError::InvalidTransition),
            "expiry during the injector call must conservatively fence to in_doubt"
        );
        assert_eq!(
            lookup_at(
                during_dir.path(),
                &during_binding,
                started_at + (OWNER_LEASE_DURATION_MS * 2),
            ),
            Ok(Some(StoredSubmitState::InDoubt))
        );
    }

    #[test]
    fn implausibly_future_lease_clock_edge_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "clock-edge");
        let now = 30_000;
        let _token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [6; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            now,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        raw_connection(dir.path())
            .execute(
                "UPDATE verified_submit_idempotency SET lease_expires_unix_ms = ?1 WHERE idempotency_key COLLATE BINARY = ?2 COLLATE BINARY",
                params![now + MAX_OWNER_LEASE_FUTURE_MS + 1, binding.key()],
            )
            .expect("inject future clock edge");
        assert_eq!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &binding,
                [7; OWNER_NONCE_BYTES],
                PRODUCTION_LIMITS,
                now,
            ),
            Ok(ClaimOutcome::InDoubt)
        );
    }

    #[test]
    fn claim_that_finishes_after_its_lease_never_returns_an_owner_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "slow-claim");
        let mut clock_values = [40_000, 40_000 + OWNER_LEASE_DURATION_MS].into_iter();
        let outcome = claim_with_nonce_limits_and_clock(
            dir.path(),
            &binding,
            [8; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            || clock_values.next().expect("claim clock sample"),
        )
        .expect("slow claim should fail closed");
        assert_eq!(outcome, ClaimOutcome::InDoubt);
        assert_eq!(
            lookup_at(
                dir.path(),
                &binding,
                40_000 + OWNER_LEASE_DURATION_MS,
            ),
            Ok(Some(StoredSubmitState::InDoubt))
        );
    }

    #[test]
    fn backward_clock_reclaim_preserves_monotonic_persisted_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "backward-reclaim");
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [9; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            1_000,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        transition_from_active_owner_at(
            dir.path(),
            &binding,
            token,
            STATE_RETRYABLE,
            Some(RetryableReason::PolicyDenied),
            1_100,
        )
        .expect("mark retryable");
        assert!(matches!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &binding,
                [10; OWNER_NONCE_BYTES],
                PRODUCTION_LIMITS,
                900,
            ),
            Ok(ClaimOutcome::Claimed(_))
        ));
        assert_eq!(persisted_updated_at(dir.path(), &binding), 1_100);
    }

    #[test]
    fn backward_clock_expiry_preserves_monotonic_persisted_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "backward-expiry");
        let _token = claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [11; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            1_000,
        )
        .expect("claim");
        raw_connection(dir.path())
            .execute(
                "UPDATE verified_submit_idempotency SET lease_expires_unix_ms = 800 WHERE idempotency_key = ?1",
                [binding.key()],
            )
            .expect("simulate an expired lease after a wall-clock step");
        assert_eq!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &binding,
                [12; OWNER_NONCE_BYTES],
                PRODUCTION_LIMITS,
                900,
            ),
            Ok(ClaimOutcome::InDoubt)
        );
        assert_eq!(persisted_updated_at(dir.path(), &binding), 1_000);
    }

    #[test]
    fn backward_clock_terminal_transition_preserves_persisted_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "backward-transition");
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [13; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            1_000,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        transition_from_active_owner_at(
            dir.path(),
            &binding,
            token,
            STATE_EFFECT_APPLIED_RECEIPT_PENDING,
            None,
            900,
        )
        .expect("backward-clock pending transition");
        assert_eq!(persisted_updated_at(dir.path(), &binding), 1_000);
    }

    #[test]
    fn backward_clock_renewal_preserves_persisted_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "backward-renewal");
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [14; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            1_000,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        renew_claim_at(dir.path(), &binding, token, 900).expect("backward-clock renewal");
        assert_eq!(persisted_updated_at(dir.path(), &binding), 1_000);
    }

    #[test]
    fn backward_clock_completion_preserves_persisted_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(8, "backward-completion");
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [15; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            1_000,
        )
        .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        transition_from_active_owner_at(
            dir.path(),
            &binding,
            token,
            STATE_EFFECT_APPLIED_RECEIPT_PENDING,
            None,
            1_100,
        )
        .expect("pending transition");
        complete_at(
            dir.path(),
            &binding,
            token,
            &receipt(&binding, SubmitReceiptState::Submitted),
            900,
        )
        .expect("backward-clock completion");
        assert_eq!(persisted_updated_at(dir.path(), &binding), 1_100);
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
                .filter(|outcome| matches!(outcome, ClaimOutcome::InFlight))
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
        assert_eq!(claim(dir.path(), &binding), Ok(ClaimOutcome::InFlight));
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
        let submitted = receipt(&binding, SubmitReceiptState::Submitted);
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
        complete_owned(dir.path(), &binding, second_token, &submitted);
    }

    #[test]
    fn owner_nonce_fences_row_and_database_recreation_aba() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(10, "nonce-aba");
        let old_token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [1; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            10_000,
        )
        .expect("initial claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        let debug = format!("{old_token:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("[1, 1"));

        raw_connection(dir.path())
            .execute(
                "DELETE FROM verified_submit_idempotency WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY",
                [binding.key()],
            )
            .expect("recreate row fixture");
        let recreated_row_token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [2; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            10_001,
        )
        .expect("claim recreated row")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected recreated owner, got {other:?}"),
        };
        assert_eq!(old_token.generation, recreated_row_token.generation);
        assert_eq!(
            transition_from_active_owner_at(
                dir.path(),
                &binding,
                old_token,
                STATE_IN_DOUBT,
                None,
                10_002,
            ),
            Err(SubmitIdempotencyError::InvalidTransition)
        );

        let conn = raw_connection(dir.path());
        conn.execute_batch(
            "DROP TABLE verified_submit_idempotency; PRAGMA application_id = 0; PRAGMA user_version = 0;",
        )
        .expect("recreate database schema fixture");
        drop(conn);
        let recreated_db_token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [3; OWNER_NONCE_BYTES],
            PRODUCTION_LIMITS,
            10_003,
        )
        .expect("claim recreated database")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected recreated database owner, got {other:?}"),
        };
        assert_eq!(old_token.generation, recreated_db_token.generation);
        assert_eq!(
            renew_claim_at(dir.path(), &binding, old_token, 10_004),
            Err(SubmitIdempotencyError::InvalidTransition)
        );
        renew_claim_at(dir.path(), &binding, recreated_db_token, 10_004)
            .expect("current recreated owner renews");
    }

    #[test]
    fn global_capacity_reserves_every_ambiguous_receipt_without_eviction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = binding(10, "capacity-first");
        let second = binding(10, "capacity-second");
        let record_bytes = new_record_logical_bytes(&first, PRODUCTION_LIMITS)
            .expect("bounded record size");
        let limits = StoreLimits {
            max_records: 10,
            max_logical_bytes: record_bytes
                .checked_mul(2)
                .and_then(|value| value.checked_sub(1))
                .expect("test capacity"),
            receipt_reserve_bytes: MAX_RECEIPT_JSON_BYTES as i64,
        };
        let first_token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &first,
            [1; OWNER_NONCE_BYTES],
            limits,
            1_000,
        )
        .expect("first claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected first owner, got {other:?}"),
        };
        transition_from_active_owner_at(
            dir.path(),
            &first,
            first_token,
            STATE_IN_DOUBT,
            None,
            1_001,
        )
        .expect("mark first in doubt");
        assert_eq!(
            claim_with_nonce_limits_and_time(
                dir.path(),
                &second,
                [2; OWNER_NONCE_BYTES],
                limits,
                1_002,
            ),
            Err(SubmitIdempotencyError::CapacityExceeded)
        );
        assert_eq!(
            lookup_at(dir.path(), &first, 1_003),
            Ok(Some(StoredSubmitState::InDoubt)),
            "capacity pressure must never evict an ambiguous effect"
        );
        assert!(!SubmitIdempotencyError::CapacityExceeded.is_retryable());
    }

    #[test]
    fn reserved_headroom_allows_terminal_receipt_without_overcommit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(10, "terminal-headroom");
        let exact_capacity = new_record_logical_bytes(&binding, PRODUCTION_LIMITS)
            .expect("bounded record size");
        let limits = StoreLimits {
            max_records: 1,
            max_logical_bytes: exact_capacity,
            receipt_reserve_bytes: MAX_RECEIPT_JSON_BYTES as i64,
        };
        let token = match claim_with_nonce_limits_and_time(
            dir.path(),
            &binding,
            [9; OWNER_NONCE_BYTES],
            limits,
            2_000,
        )
        .expect("claim with exact headroom")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected owner, got {other:?}"),
        };
        transition_from_active_owner_at(
            dir.path(),
            &binding,
            token,
            STATE_EFFECT_APPLIED_RECEIPT_PENDING,
            None,
            2_001,
        )
        .expect("mark effect pending");
        let receipt = receipt(&binding, SubmitReceiptState::Submitted);
        complete(dir.path(), &binding, token, &receipt).expect("reserved receipt completes");
        assert_eq!(
            claim(dir.path(), &binding),
            Ok(ClaimOutcome::Completed(receipt))
        );
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
                complete(dir.path(), &binding, token, &receipt(&binding, state)),
                Err(SubmitIdempotencyError::ReceiptInvalid)
            );
            assert_eq!(
                lookup(dir.path(), &binding),
                Ok(Some(StoredSubmitState::ActiveOwner)),
                "a rejected completion must not mutate the claim"
            );
        }
    }

    #[test]
    fn mismatched_pane_request_and_schema_fail_closed() {
        for (suffix, column, replacement, expected) in [
            (
                "pane",
                "pane_id",
                "999",
                SubmitIdempotencyError::RecordCorrupt,
            ),
            (
                "request",
                "request_sha256",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                SubmitIdempotencyError::RequestConflict,
            ),
            (
                "effect",
                "effect_sha256",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                SubmitIdempotencyError::RequestConflict,
            ),
            (
                "schema",
                "schema_version",
                "999",
                SubmitIdempotencyError::RecordCorrupt,
            ),
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
                Err(expected),
                "{column} mismatch must fail closed"
            );
        }
    }

    #[test]
    fn malformed_and_oversize_completed_receipts_fail_before_allocation() {
        let invalid_dir = tempfile::tempdir().expect("tempdir");
        let invalid_binding = binding(12, "invalid-json");
        let _token = claim_token(invalid_dir.path(), &invalid_binding);
        let conn = raw_connection(invalid_dir.path());
        conn.execute(
            "UPDATE verified_submit_idempotency SET state = ?1, receipt_json = ?2, lease_expires_unix_ms = NULL \
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
            "UPDATE verified_submit_idempotency SET state = ?1, receipt_json = ?2, lease_expires_unix_ms = NULL \
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
            "internal_claim_key",
            "pane_id",
            "request_sha256",
            "effect_sha256",
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let request_binding = binding(13, field);
            let token = claim_token(dir.path(), &request_binding);
            mark_effect_applied_receipt_pending(dir.path(), &request_binding, token)
                .expect("mark pending");
            complete(
                dir.path(),
                &request_binding,
                token,
                &receipt(&request_binding, SubmitReceiptState::Submitted),
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
                "schema_version" => serde_json::json!(RECEIPT_SCHEMA_VERSION + 1),
                "internal_claim_key" => serde_json::json!(binding(99, "other").key()),
                "pane_id" => serde_json::json!(99),
                "request_sha256" => serde_json::json!(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                ),
                "effect_sha256" => serde_json::json!(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
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
        mark_effect_applied_receipt_pending(dir.path(), &request_binding, token)
            .expect("mark pending");
        complete(
            dir.path(),
            &request_binding,
            token,
            &receipt(&request_binding, SubmitReceiptState::Submitted),
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
        value["receipt"]["unexpected"] = serde_json::json!(true);
        conn.execute(
            "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
             WHERE idempotency_key = ?2",
            params![value.to_string(), request_binding.key()],
        )
        .expect("add unknown receipt field");
        assert_eq!(
            lookup(dir.path(), &request_binding),
            Err(SubmitIdempotencyError::ReceiptInvalid)
        );

        value["receipt"]
            .as_object_mut()
            .expect("stored receipt object")
            .remove("unexpected");
        value["receipt"]["state"] = serde_json::json!("policy_denied");
        conn.execute(
            "UPDATE verified_submit_idempotency SET receipt_json = ?1 \
             WHERE idempotency_key = ?2",
            params![value.to_string(), request_binding.key()],
        )
        .expect("substitute pre-effect completed state");
        assert_eq!(
            lookup(dir.path(), &request_binding),
            Err(SubmitIdempotencyError::ReceiptInvalid)
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
    fn initialized_database_requires_the_binary_ordered_request_lookup_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(13, "missing-request-index");
        let _token = claim_token(dir.path(), &binding);
        raw_connection(dir.path())
            .execute_batch("DROP INDEX verified_submit_idempotency_request_lookup")
            .expect("corrupt request lookup fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
        raw_connection(dir.path())
            .execute_batch(
                "CREATE INDEX verified_submit_idempotency_request_lookup \
                 ON verified_submit_idempotency (request_sha256 COLLATE NOCASE, pane_id COLLATE BINARY)",
            )
            .expect("install reversed non-binary request lookup fixture");
        assert_eq!(
            lookup(dir.path(), &binding),
            Err(SubmitIdempotencyError::SchemaMismatch),
            "request lookup order and BINARY collation are schema authority"
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
    fn application_id_and_blank_unrelated_database_fail_closed() {
        let app_dir = tempfile::tempdir().expect("tempdir");
        let app_binding = binding(13, "application-id");
        let _token = claim_token(app_dir.path(), &app_binding);
        raw_connection(app_dir.path())
            .pragma_update(None, "application_id", STORE_APPLICATION_ID + 1)
            .expect("corrupt application id fixture");
        assert_eq!(
            lookup(app_dir.path(), &app_binding),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );

        let unrelated_dir = tempfile::tempdir().expect("tempdir");
        let conn = raw_connection(unrelated_dir.path());
        conn.execute_batch("CREATE TABLE unrelated(value INTEGER)")
            .expect("create unrelated blank-header schema");
        drop(conn);
        assert_eq!(
            claim(unrelated_dir.path(), &binding(13, "unrelated")),
            Err(SubmitIdempotencyError::SchemaMismatch)
        );
        let journal_mode: String = raw_connection(unrelated_dir.path())
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read unrelated database journal mode");
        assert_eq!(
            journal_mode.to_ascii_lowercase(),
            "delete",
            "schema preflight must reject an unrelated database before WAL mutates it"
        );
    }

    #[test]
    fn incoming_oversize_report_leaves_conservative_in_doubt_claim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(14, "oversize-input");
        let token = claim_token(dir.path(), &binding);
        let mut oversized = receipt(&binding, SubmitReceiptState::Submitted);
        oversized.evidence_rule_ids = vec!["x".repeat(MAX_RECEIPT_FIELD_BYTES + 1)];
        mark_effect_applied_receipt_pending(dir.path(), &binding, token).expect("mark pending");
        assert_eq!(
            complete(dir.path(), &binding, token, &oversized),
            Err(SubmitIdempotencyError::ReceiptOversize)
        );
        assert_eq!(
            claim(dir.path(), &binding),
            Ok(ClaimOutcome::EffectAppliedReceiptPending)
        );
    }

    #[test]
    fn every_legacy_store_shape_fails_closed_without_migration() {
        let file_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(file_dir.path().join(LEGACY_STORE_NAME), b"legacy sentinel")
            .expect("legacy file fixture");
        assert_eq!(
            claim(file_dir.path(), &binding(15, "legacy-file")),
            Err(SubmitIdempotencyError::LegacyStorePresent)
        );
        assert!(!database_path(file_dir.path()).exists());

        let directory_dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory_dir.path().join(LEGACY_STORE_NAME))
            .expect("legacy directory fixture");
        assert_eq!(
            lookup(directory_dir.path(), &binding(15, "legacy-directory")),
            Err(SubmitIdempotencyError::LegacyStorePresent)
        );
        assert!(!database_path(directory_dir.path()).exists());
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

    #[cfg(unix)]
    #[test]
    fn sqlite_journal_sidecar_symlinks_are_rejected_before_database_creation() {
        use std::os::unix::fs::symlink;

        for (index, filename) in STORE_AUXILIARY_FILENAMES.into_iter().enumerate() {
            let dir = tempfile::tempdir().expect("tempdir");
            let target = dir.path().join(format!("sidecar-target-{index}"));
            std::fs::write(&target, b"must remain untouched").expect("write sidecar target");
            symlink(&target, dir.path().join(filename)).expect("create sidecar symlink");
            assert_eq!(
                claim(dir.path(), &binding(15, filename)),
                Err(SubmitIdempotencyError::SymlinkRejected),
                "sidecar {filename} must be rejected"
            );
            assert_eq!(
                std::fs::read(&target).expect("read sidecar target"),
                b"must remain untouched"
            );
            assert!(
                !database_path(dir.path()).exists(),
                "sidecar rejection must precede database creation"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_and_known_journal_sidecars_are_hardened_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let binding = binding(15, "private-store-files");
        let _token = claim_token(dir.path(), &binding);
        assert_eq!(
            std::fs::metadata(database_path(dir.path()))
                .expect("database metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        for filename in STORE_AUXILIARY_FILENAMES {
            let path = dir.path().join(filename);
            std::fs::write(&path, b"sidecar fixture").expect("write sidecar fixture");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                .expect("make sidecar fixture permissive");
        }
        let pinned = CapDir::open_ambient_dir(dir.path(), cap_std::ambient_authority())
            .expect("pin test store directory");
        harden_store_auxiliary_leaves(&pinned).expect("harden sidecar fixtures");
        for filename in STORE_AUXILIARY_FILENAMES {
            assert_eq!(
                std::fs::metadata(dir.path().join(filename))
                    .expect("sidecar metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "sidecar {filename} must be owner-only"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_and_ancestor_directory_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let legacy_dir = tempfile::tempdir().expect("tempdir");
        let legacy_target = legacy_dir.path().join("legacy-target");
        std::fs::write(&legacy_target, b"legacy sentinel").expect("write legacy target");
        symlink(&legacy_target, legacy_dir.path().join(LEGACY_STORE_NAME))
            .expect("legacy symlink fixture");
        assert_eq!(
            claim(legacy_dir.path(), &binding(15, "legacy-symlink")),
            Err(SubmitIdempotencyError::LegacyStorePresent)
        );

        let ancestor_root = tempfile::tempdir().expect("tempdir");
        let real_parent = ancestor_root.path().join("real-parent");
        std::fs::create_dir(&real_parent).expect("real parent");
        let linked_parent = ancestor_root.path().join("linked-parent");
        symlink(&real_parent, &linked_parent).expect("ancestor symlink fixture");
        let nested_store = linked_parent.join("nested-ft");
        assert_eq!(
            claim(&nested_store, &binding(15, "ancestor-symlink")),
            Err(SubmitIdempotencyError::SymlinkRejected)
        );
        assert!(!real_parent.join("nested-ft").exists());
    }
}
