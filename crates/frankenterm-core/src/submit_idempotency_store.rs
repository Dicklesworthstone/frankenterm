//! Durable claim state machine for verified-submit idempotency.
//!
//! A claim is committed as `in_doubt` *before* policy-gated injection can have
//! any side effect. A concurrent caller for the same fully bound request cannot
//! claim the unique SQLite row, and an interrupted owner is never
//! automatically retried. This provides at-most-once automatic effect under
//! ambiguity; it deliberately does not claim exactly-once delivery.

use crate::robot_types::SubmitReceiptState;
use crate::verified_submit::{SubmitIdempotencyBinding, VerifiedSubmitReport};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STORE_FILENAME: &str = "submit_idempotency.sqlite3";
const STORE_SCHEMA_VERSION: i64 = 1;
const STATE_IN_DOUBT: i64 = 1;
const STATE_COMPLETED: i64 = 2;
const STATE_RETRYABLE: i64 = 3;
const RETRYABLE_POLICY_DENIED: i64 = 1;
const RETRYABLE_APPROVAL_REQUIRED: i64 = 2;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RECEIPT_JSON_BYTES: usize = 64 * 1024;
const MAX_REPORT_EVIDENCE_ITEMS: usize = 64;
const MAX_REPORT_FIELD_BYTES: usize = 1024;
const MAX_REPORT_CURSOR_BYTES: usize = 512;

const CREATE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS verified_submit_idempotency (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    schema_version INTEGER NOT NULL,
    pane_id TEXT NOT NULL,
    request_sha256 TEXT NOT NULL,
    state INTEGER NOT NULL CHECK (state IN (1, 2, 3)),
    retryable_reason INTEGER,
    receipt_json TEXT,
    generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
    created_unix_ms INTEGER NOT NULL,
    updated_unix_ms INTEGER NOT NULL,
    CHECK (length(idempotency_key) BETWEEN 71 AND 90),
    CHECK (length(pane_id) BETWEEN 1 AND 20),
    CHECK (length(request_sha256) = 64),
    CHECK (receipt_json IS NULL OR length(CAST(receipt_json AS BLOB)) <= 65536),
    CHECK (
        (state = 1 AND retryable_reason IS NULL AND receipt_json IS NULL) OR
        (state = 2 AND retryable_reason IS NULL AND receipt_json IS NOT NULL) OR
        (state = 3 AND retryable_reason IN (1, 2) AND receipt_json IS NULL)
    )
) STRICT, WITHOUT ROWID;
CREATE UNIQUE INDEX IF NOT EXISTS verified_submit_idempotency_request_unique
    ON verified_submit_idempotency (pane_id, request_sha256);
"#;

/// Finite failure taxonomy. No variant retains a filesystem path, SQL string,
/// serialized receipt, or backend error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitIdempotencyError {
    #[error("submit idempotency binding is invalid")]
    InvalidBinding,
    #[error("submit idempotency database path is a symbolic link")]
    SymlinkRejected,
    #[error("submit idempotency database directory is unavailable")]
    DirectoryUnavailable,
    #[error("submit idempotency database open failed")]
    OpenFailed,
    #[error("submit idempotency database configuration failed")]
    ConfigurationFailed,
    #[error("submit idempotency schema is unsupported")]
    SchemaMismatch,
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
            Self::SymlinkRejected => "symlink_rejected",
            Self::DirectoryUnavailable => "directory_unavailable",
            Self::OpenFailed => "open_failed",
            Self::ConfigurationFailed => "configuration_failed",
            Self::SchemaMismatch => "schema_mismatch",
            Self::ClaimFailed => "claim_failed",
            Self::TransitionFailed => "transition_failed",
            Self::MissingClaim => "missing_claim",
            Self::InvalidTransition => "invalid_transition",
            Self::RecordCorrupt => "record_corrupt",
            Self::ReceiptOversize => "receipt_oversize",
            Self::ReceiptInvalid => "receipt_invalid",
        }
    }
}

/// A successful unique claim, a completed replay, or a conservative refusal to
/// retry an owner whose side-effect outcome is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ClaimOutcome {
    Claimed(ClaimToken),
    Completed(VerifiedSubmitReport),
    InDoubt,
}

/// Opaque ownership generation returned by [`claim`]. Every terminal
/// transition must present the token so a stale owner cannot complete or reopen
/// a later claimant's generation (the retryable-state ABA guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimToken {
    generation: i64,
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
    InDoubt,
    Completed(VerifiedSubmitReport),
    Retryable(RetryableReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceiptEnvelope {
    schema_version: i64,
    idempotency_key: String,
    pane_id: u64,
    request_sha256: String,
    report: StoredVerifiedSubmitReport,
}

/// Storage-local mirror so unknown/missing receipt fields cannot be silently
/// accepted by serde when reading an authority-bearing completed row. Any
/// change to this shape requires a store-schema migration/version bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredVerifiedSubmitReport {
    state: SubmitReceiptState,
    agent_type: Option<String>,
    profile_id: Option<String>,
    profile_version: Option<String>,
    attempts: u32,
    evidence_rule_ids: Vec<String>,
    polls: usize,
    cursor_before: Option<String>,
    cursor_after: Option<String>,
}

impl From<&VerifiedSubmitReport> for StoredVerifiedSubmitReport {
    fn from(report: &VerifiedSubmitReport) -> Self {
        Self {
            state: report.state,
            agent_type: report.agent_type.clone(),
            profile_id: report.profile_id.clone(),
            profile_version: report.profile_version.clone(),
            attempts: report.attempts,
            evidence_rule_ids: report.evidence_rule_ids.clone(),
            polls: report.polls,
            cursor_before: report.cursor_before.clone(),
            cursor_after: report.cursor_after.clone(),
        }
    }
}

impl From<StoredVerifiedSubmitReport> for VerifiedSubmitReport {
    fn from(report: StoredVerifiedSubmitReport) -> Self {
        Self {
            state: report.state,
            agent_type: report.agent_type,
            profile_id: report.profile_id,
            profile_version: report.profile_version,
            attempts: report.attempts,
            evidence_rule_ids: report.evidence_rule_ids,
            polls: report.polls,
            cursor_before: report.cursor_before,
            cursor_after: report.cursor_after,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StoredHeader {
    state: i64,
    retryable_reason: Option<i64>,
    generation: i64,
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
    if binding.is_canonical()
        && is_valid_submit_key(binding.key())
        && binding
            .key()
            .strip_prefix("idem:")
            .and_then(|rest| rest.split_once(':'))
            .and_then(|(pane, _)| pane.parse::<u64>().ok())
            == Some(binding.pane_id())
    {
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

fn validate_initialized_schema(conn: &Connection) -> Result<(), SubmitIdempotencyError> {
    let (without_rowid, strict) = conn
        .query_row(
            "SELECT wr, strict FROM pragma_table_list \
             WHERE schema = 'main' AND name = 'verified_submit_idempotency'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|_| SubmitIdempotencyError::SchemaMismatch)?;
    let (columns, matching_columns) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE \
                 WHEN cid = 0 AND name = 'idempotency_key' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 1 AND hidden = 0 THEN 1 \
                 WHEN cid = 1 AND name = 'schema_version' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 2 AND name = 'pane_id' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 3 AND name = 'request_sha256' AND type = 'TEXT' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 4 AND name = 'state' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 5 AND name = 'retryable_reason' AND type = 'INTEGER' AND \"notnull\" = 0 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 6 AND name = 'receipt_json' AND type = 'TEXT' AND \"notnull\" = 0 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 7 AND name = 'generation' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 8 AND name = 'created_unix_ms' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 WHEN cid = 9 AND name = 'updated_unix_ms' AND type = 'INTEGER' AND \"notnull\" = 1 AND pk = 0 AND hidden = 0 THEN 1 \
                 ELSE 0 END), 0) \
             FROM pragma_table_xinfo('verified_submit_idempotency')",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|_| SubmitIdempotencyError::SchemaMismatch)?;
    let (triggers, foreign_keys) = conn
        .query_row(
            "SELECT \
                 (SELECT COUNT(*) FROM main.sqlite_schema \
                  WHERE type = 'trigger' AND tbl_name = 'verified_submit_idempotency'), \
                 (SELECT COUNT(*) \
                  FROM pragma_foreign_key_list('verified_submit_idempotency'))",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|_| SubmitIdempotencyError::SchemaMismatch)?;
    let (index_unique, index_origin, index_partial) = conn
        .query_row(
            "SELECT \"unique\", origin, partial \
             FROM pragma_index_list('verified_submit_idempotency') \
             WHERE name = 'verified_submit_idempotency_request_unique'",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|_| SubmitIdempotencyError::SchemaMismatch)?;
    let (index_columns, matching_index_columns) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE \
                 WHEN seqno = 0 AND cid = 2 AND name = 'pane_id' THEN 1 \
                 WHEN seqno = 1 AND cid = 3 AND name = 'request_sha256' THEN 1 \
                 ELSE 0 END), 0) \
             FROM pragma_index_info('verified_submit_idempotency_request_unique')",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|_| SubmitIdempotencyError::SchemaMismatch)?;
    if without_rowid == 1
        && strict == 1
        && columns == 10
        && matching_columns == 10
        && index_unique == 1
        && index_origin == "c"
        && index_partial == 0
        && index_columns == 2
        && matching_index_columns == 2
        && triggers == 0
        && foreign_keys == 0
    {
        Ok(())
    } else {
        Err(SubmitIdempotencyError::SchemaMismatch)
    }
}

fn open_store(ft_dir: &Path) -> Result<Connection, SubmitIdempotencyError> {
    std::fs::create_dir_all(ft_dir)
        .map_err(|_| SubmitIdempotencyError::DirectoryUnavailable)?;
    let path = database_path(ft_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(SubmitIdempotencyError::SymlinkRejected);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SubmitIdempotencyError::OpenFailed),
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut conn = Connection::open_with_flags(path, flags)
        .map_err(|_| SubmitIdempotencyError::OpenFailed)?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    conn.pragma_update(None, "fullfsync", true)
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    conn.pragma_update(None, "checkpoint_fullfsync", true)
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    conn.pragma_update(None, "trusted_schema", false)
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;

    let version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    if !matches!(version, 0 | STORE_SCHEMA_VERSION) {
        return Err(SubmitIdempotencyError::SchemaMismatch);
    }
    if version == 0 {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
        tx.execute_batch(CREATE_SCHEMA_SQL)
            .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
        validate_initialized_schema(&tx)?;
        tx.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)
            .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
        tx.commit()
            .map_err(|_| SubmitIdempotencyError::ConfigurationFailed)?;
    }
    validate_initialized_schema(&conn)?;
    Ok(conn)
}

fn read_header(
    conn: &Connection,
    binding: &SubmitIdempotencyBinding,
) -> Result<Option<StoredHeader>, SubmitIdempotencyError> {
    let pane = binding.pane_id().to_string();
    let row = conn
        .query_row(
            "SELECT schema_version, pane_id = ?2, request_sha256 = ?3, \
                    length(pane_id), length(request_sha256), state, \
                    retryable_reason, length(CAST(receipt_json AS BLOB)), generation \
             FROM verified_submit_idempotency WHERE idempotency_key = ?1",
            params![binding.key(), pane, binding.request_sha256()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
    let Some((
        schema,
        pane_matches,
        request_matches,
        pane_len,
        digest_len,
        state,
        reason,
        bytes,
        generation,
    )) = row
    else {
        let request_row_exists = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM verified_submit_idempotency \
                 WHERE pane_id = ?1 AND request_sha256 = ?2)",
                params![pane, binding.request_sha256()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
        return if request_row_exists == 0 {
            Ok(None)
        } else {
            Err(SubmitIdempotencyError::RecordCorrupt)
        };
    };
    let expected_pane_len = i64::try_from(binding.pane_id().to_string().len()).unwrap_or(i64::MAX);
    if schema != STORE_SCHEMA_VERSION
        || pane_matches != 1
        || request_matches != 1
        || pane_len != expected_pane_len
        || digest_len != 64
        || generation < 1
    {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    let shape_ok = match state {
        STATE_IN_DOUBT => reason.is_none() && bytes.is_none(),
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
    Ok(Some(StoredHeader {
        state,
        retryable_reason: reason,
        generation,
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

fn validate_report(report: &VerifiedSubmitReport) -> Result<(), SubmitIdempotencyError> {
    validate_optional_field(report.agent_type.as_deref(), MAX_REPORT_FIELD_BYTES)?;
    validate_optional_field(report.profile_id.as_deref(), MAX_REPORT_FIELD_BYTES)?;
    validate_optional_field(report.profile_version.as_deref(), MAX_REPORT_FIELD_BYTES)?;
    validate_optional_field(report.cursor_before.as_deref(), MAX_REPORT_CURSOR_BYTES)?;
    validate_optional_field(report.cursor_after.as_deref(), MAX_REPORT_CURSOR_BYTES)?;
    if report.evidence_rule_ids.len() > MAX_REPORT_EVIDENCE_ITEMS
        || report
            .evidence_rule_ids
            .iter()
            .any(|item| item.len() > MAX_REPORT_FIELD_BYTES)
    {
        return Err(SubmitIdempotencyError::ReceiptOversize);
    }
    Ok(())
}

fn serialize_receipt(
    binding: &SubmitIdempotencyBinding,
    report: &VerifiedSubmitReport,
) -> Result<String, SubmitIdempotencyError> {
    validate_report(report)?;
    let envelope = StoredReceiptEnvelope {
        schema_version: STORE_SCHEMA_VERSION,
        idempotency_key: binding.key().to_string(),
        pane_id: binding.pane_id(),
        request_sha256: binding.request_sha256().to_string(),
        report: report.into(),
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
) -> Result<VerifiedSubmitReport, SubmitIdempotencyError> {
    let json = conn
        .query_row(
            "SELECT receipt_json FROM verified_submit_idempotency WHERE idempotency_key = ?1",
            [binding.key()],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| SubmitIdempotencyError::RecordCorrupt)?;
    if json.len() > MAX_RECEIPT_JSON_BYTES {
        return Err(SubmitIdempotencyError::ReceiptOversize);
    }
    let envelope: StoredReceiptEnvelope =
        serde_json::from_str(&json).map_err(|_| SubmitIdempotencyError::ReceiptInvalid)?;
    if envelope.schema_version != STORE_SCHEMA_VERSION
        || envelope.idempotency_key != binding.key()
        || envelope.pane_id != binding.pane_id()
        || envelope.request_sha256 != binding.request_sha256()
    {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    let report = VerifiedSubmitReport::from(envelope.report);
    validate_report(&report)?;
    if matches!(
        report.state,
        SubmitReceiptState::PolicyDenied | SubmitReceiptState::RequiresApproval
    ) {
        return Err(SubmitIdempotencyError::RecordCorrupt);
    }
    Ok(report)
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
    validate_binding(binding)?;
    let mut conn = open_store(ft_dir)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
    match read_header(&tx, binding)? {
        None => {
            let now = now_unix_ms();
            tx.execute(
                "INSERT INTO verified_submit_idempotency \
                 (idempotency_key, schema_version, pane_id, request_sha256, state, \
                  retryable_reason, receipt_json, generation, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, 1, ?6, ?6)",
                params![
                    binding.key(),
                    STORE_SCHEMA_VERSION,
                    binding.pane_id().to_string(),
                    binding.request_sha256(),
                    STATE_IN_DOUBT,
                    now,
                ],
            )
            .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            tx.commit()
                .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Claimed(ClaimToken { generation: 1 }))
        }
        Some(header) if header.state == STATE_IN_DOUBT => {
            tx.commit()
                .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::InDoubt)
        }
        Some(header) if header.state == STATE_COMPLETED => {
            let report = load_receipt(&tx, binding)?;
            tx.commit()
                .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Completed(report))
        }
        Some(header) if header.state == STATE_RETRYABLE => {
            let next_generation = header
                .generation
                .checked_add(1)
                .ok_or(SubmitIdempotencyError::RecordCorrupt)?;
            let changed = tx
                .execute(
                    "UPDATE verified_submit_idempotency \
                     SET state = ?2, retryable_reason = NULL, receipt_json = NULL, \
                         generation = ?3, updated_unix_ms = ?4 \
                     WHERE idempotency_key = ?1 AND state = ?5 AND generation = ?6",
                    params![
                        binding.key(),
                        STATE_IN_DOUBT,
                        next_generation,
                        now_unix_ms(),
                        STATE_RETRYABLE,
                        header.generation,
                    ],
                )
                .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            if changed != 1 {
                return Err(SubmitIdempotencyError::ClaimFailed);
            }
            tx.commit()
                .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
            Ok(ClaimOutcome::Claimed(ClaimToken {
                generation: next_generation,
            }))
        }
        Some(_) => Err(SubmitIdempotencyError::RecordCorrupt),
    }
}

/// Durably attach a bounded, fully bound receipt to an owned `in_doubt` claim.
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
    report: &VerifiedSubmitReport,
) -> Result<(), SubmitIdempotencyError> {
    validate_binding(binding)?;
    if matches!(
        report.state,
        SubmitReceiptState::PolicyDenied | SubmitReceiptState::RequiresApproval
    ) {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let receipt_json = serialize_receipt(binding, report)?;
    let mut conn = open_store(ft_dir)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
    let Some(header) = read_header(&tx, binding)? else {
        return Err(SubmitIdempotencyError::MissingClaim);
    };
    if header.generation != token.generation {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    if header.state == STATE_COMPLETED {
        let stored = load_receipt(&tx, binding)?;
        if stored != *report {
            return Err(SubmitIdempotencyError::InvalidTransition);
        }
        tx.commit()
            .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
        return Ok(());
    }
    if header.state != STATE_IN_DOUBT {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let changed = tx
        .execute(
            "UPDATE verified_submit_idempotency \
             SET state = ?2, retryable_reason = NULL, receipt_json = ?3, updated_unix_ms = ?4 \
             WHERE idempotency_key = ?1 AND state = ?5 AND generation = ?6",
            params![
                binding.key(),
                STATE_COMPLETED,
                receipt_json,
                now_unix_ms(),
                STATE_IN_DOUBT,
                token.generation,
            ],
        )
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
    if changed != 1 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    tx.commit()
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)
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
    validate_binding(binding)?;
    let mut conn = open_store(ft_dir)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
    let Some(header) = read_header(&tx, binding)? else {
        return Err(SubmitIdempotencyError::MissingClaim);
    };
    if header.generation != token.generation {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    if header.state == STATE_RETRYABLE && header.retryable_reason == Some(reason.as_db()) {
        tx.commit()
            .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
        return Ok(());
    }
    if header.state != STATE_IN_DOUBT {
        return Err(SubmitIdempotencyError::InvalidTransition);
    }
    let changed = tx
        .execute(
            "UPDATE verified_submit_idempotency \
             SET state = ?2, retryable_reason = ?3, receipt_json = NULL, updated_unix_ms = ?4 \
             WHERE idempotency_key = ?1 AND state = ?5 AND generation = ?6",
            params![
                binding.key(),
                STATE_RETRYABLE,
                reason.as_db(),
                now_unix_ms(),
                STATE_IN_DOUBT,
                token.generation,
            ],
        )
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)?;
    if changed != 1 {
        return Err(SubmitIdempotencyError::TransitionFailed);
    }
    tx.commit()
        .map_err(|_| SubmitIdempotencyError::TransitionFailed)
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
    match std::fs::symlink_metadata(database_path(ft_dir)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(_) => {}
        Err(_) => return Err(SubmitIdempotencyError::OpenFailed),
    }
    let mut conn = open_store(ft_dir)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
    let state = match read_header(&tx, binding)? {
        None => None,
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
    tx.commit()
        .map_err(|_| SubmitIdempotencyError::ClaimFailed)?;
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
