//! Durable pre-effect ownership and completed receipts for fleet mutations.
//!
//! The `ft robot fleet scale` and `ft robot fleet rebalance` commit paths use
//! these tables to reserve execution before a mux/work-queue effect and retain
//! its result across process restarts. A claim never expires or grants another
//! execution attempt: an interrupted owner leaves an explicitly indeterminate
//! request. Receipt finalization is a separate short transaction, so no database
//! writer lock is held while the executor operates on the mux or work queue.
//!
//! Schema lives in storage/migrations.rs at versions 27 and 50. Legacy receipt
//! rows remain readable, but cannot establish a missing request binding.

use rand::{TryRng, rngs::SysRng};

use crate::fleet_mutation::{FleetMutationPlan, FleetMutationReceipt, FleetMutationRequestBinding};
use crate::storage_backend_helpers::execute_typed;
use crate::storage_backend_row_helpers::RowReader;
use crate::storage_backend_trait::{BackendError, StorageBackend, StorageTransaction, ToSqlValue};
#[cfg(test)]
use crate::storage_backend_trait::{OpenConfig, RusqliteBackend};

pub const FLEET_MUTATION_RECEIPTS_SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS fleet_mutation_receipts (
    idempotency_key     TEXT PRIMARY KEY NOT NULL,
    payload_fingerprint TEXT NOT NULL,
    action              TEXT NOT NULL,
    plan_id             TEXT NOT NULL,
    dry_run             INTEGER NOT NULL DEFAULT 0,
    receipt_json        TEXT NOT NULL,
    recorded_at_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS fleet_mutation_receipts_action_time_idx
    ON fleet_mutation_receipts(action, recorded_at_ms DESC);
";

/// The immutable request binding is separate from the inventory-derived plan.
/// Unresolved rows deliberately have no expiry or reclamation mechanism.
pub const FLEET_MUTATION_CLAIMS_SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS fleet_mutation_claims (
    idempotency_key TEXT COLLATE BINARY PRIMARY KEY NOT NULL
        CHECK(length(CAST(idempotency_key AS BLOB)) BETWEEN 1 AND 512),
    request_sha256 TEXT COLLATE BINARY NOT NULL
        CHECK(length(request_sha256) = 64 AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
    action TEXT COLLATE BINARY NOT NULL
        CHECK(length(CAST(action AS BLOB)) BETWEEN 1 AND 512),
    payload_fingerprint TEXT COLLATE BINARY NOT NULL
        CHECK(length(CAST(payload_fingerprint AS BLOB)) BETWEEN 1 AND 512),
    plan_id TEXT COLLATE BINARY NOT NULL
        CHECK(length(CAST(plan_id AS BLOB)) BETWEEN 1 AND 512),
    owner_nonce TEXT COLLATE BINARY NOT NULL
        CHECK(length(owner_nonce) = 64 AND owner_nonce NOT GLOB '*[^0-9a-f]*'),
    state TEXT NOT NULL CHECK(state IN ('claimed', 'in_doubt', 'completed')),
    response_data_json TEXT
        CHECK(response_data_json IS NULL OR length(CAST(response_data_json AS BLOB)) <= 1048576),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(state = 'completed' OR response_data_json IS NULL)
) STRICT, WITHOUT ROWID;
";

const MAX_NEW_RECEIPT_BYTES: usize = 1_048_576;
const MAX_RESPONSE_DATA_BYTES: usize = 1_048_576;
const MAX_CLAIM_RECORDS: i64 = 16_384;
const MAX_CLAIM_LOGICAL_BYTES: i64 = 268_435_456;
const CLAIM_METADATA_RESERVE_BYTES: i64 = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetMutationReceiptRecord {
    pub idempotency_key: String,
    pub payload_fingerprint: String,
    pub action: String,
    pub plan_id: String,
    pub dry_run: bool,
    pub receipt: FleetMutationReceipt,
    pub recorded_at_ms: i64,
    /// Original response status and partial-plan information. Legacy receipts
    /// do not have this authority and therefore decode with `None`.
    pub response_data: Option<serde_json::Value>,
}

impl FleetMutationReceiptRecord {
    #[must_use]
    pub fn from_receipt(
        action: impl Into<String>,
        receipt: FleetMutationReceipt,
        recorded_at_ms: i64,
    ) -> Self {
        Self {
            idempotency_key: receipt.idempotency_key.as_str().to_string(),
            payload_fingerprint: receipt.payload_fingerprint.as_str().to_string(),
            action: action.into(),
            plan_id: receipt.plan_id.clone(),
            dry_run: receipt.dry_run,
            receipt,
            recorded_at_ms,
            response_data: None,
        }
    }
}

/// Proof of one admitted executor. Tokens are never reconstructed from a row
/// or returned to a duplicate caller, including after process interruption.
#[derive(Clone, PartialEq, Eq)]
pub struct FleetMutationClaimToken {
    idempotency_key: String,
    request_sha256: String,
    owner_nonce: String,
}

impl std::fmt::Debug for FleetMutationClaimToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetMutationClaimToken")
            .field("owner_nonce", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
// The bounded receipt stays owned by its result rather than introducing a
// second allocation at every durable replay boundary.
#[allow(clippy::large_enum_variant)]
pub enum FleetMutationClaimOutcome {
    Claimed(FleetMutationClaimToken),
    Completed(FleetMutationReceiptRecord),
    /// No second execution is authorized. This includes another live owner
    /// because persisted timestamps cannot prove process liveness.
    InDoubt,
    Conflict,
    /// A historical receipt has no authoritative immutable request binding.
    LegacyUnbound,
}

/// Error type for the fleet_mutation_receipts SQL primitives.
#[derive(Debug)]
pub enum FleetMutationReceiptsSqlError {
    /// Underlying storage backend call failed.
    Backend(BackendError),
    /// A JSON-encoded TEXT column failed to decode. Carries the column name +
    /// serde_json error message for operator diagnosis.
    Decode { column: &'static str, msg: String },
    /// Caller-side input validation failed.
    Invalid(String),
}

impl std::fmt::Display for FleetMutationReceiptsSqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(err) => {
                write!(f, "fleet_mutation_receipts storage backend error: {err}")
            }
            Self::Decode { column, msg } => {
                write!(
                    f,
                    "fleet_mutation_receipts column `{column}` decode failed: {msg}"
                )
            }
            Self::Invalid(msg) => write!(f, "fleet_mutation_receipts invalid input: {msg}"),
        }
    }
}

impl std::error::Error for FleetMutationReceiptsSqlError {}

impl From<BackendError> for FleetMutationReceiptsSqlError {
    fn from(err: BackendError) -> Self {
        Self::Backend(err)
    }
}

fn validate_text_key(
    field: &'static str,
    value: &str,
) -> Result<(), FleetMutationReceiptsSqlError> {
    if value.trim().is_empty() {
        return Err(FleetMutationReceiptsSqlError::Invalid(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > 512 {
        return Err(FleetMutationReceiptsSqlError::Invalid(format!(
            "{field} exceeds 512 bytes"
        )));
    }
    Ok(())
}

fn decode_receipt_row(
    row: &[String],
) -> Result<FleetMutationReceiptRecord, FleetMutationReceiptsSqlError> {
    let reader = RowReader::new(row);
    let receipt_json = reader.string(5)?;
    let receipt: FleetMutationReceipt = serde_json::from_str(&receipt_json).map_err(|err| {
        FleetMutationReceiptsSqlError::Decode {
            column: "receipt_json",
            msg: err.to_string(),
        }
    })?;
    Ok(FleetMutationReceiptRecord {
        idempotency_key: reader.string(0)?,
        payload_fingerprint: reader.string(1)?,
        action: reader.string(2)?,
        plan_id: reader.string(3)?,
        dry_run: reader.bool(4)?,
        receipt,
        recorded_at_ms: reader.i64(6)?,
        response_data: None,
    })
}

/// Initialize direct Robot CLI paths that bypass the main migration runner.
/// A current database must already retain its claims: recreating missing
/// execution authority would authorize interrupted requests a second time.
pub fn ensure_fleet_mutation_receipts_schema(
    backend: &dyn StorageBackend,
) -> Result<(), FleetMutationReceiptsSqlError> {
    run_claim_transaction(backend, |tx| {
        tx.execute_batch(FLEET_MUTATION_RECEIPTS_SCHEMA_SQL)?;
        // CREATE IF NOT EXISTS may be a no-op. Acquire the short writer lock
        // before reading schema/version authority, without modifying a row.
        tx.execute("UPDATE fleet_mutation_receipts SET recorded_at_ms = recorded_at_ms WHERE 0")?;
        let schema_version = tx.user_version()?;
        let load_sql = "SELECT CASE WHEN length(CAST(sql AS BLOB)) <= 65536 THEN sql END \
                        FROM sqlite_schema WHERE type = 'table' AND name = 'fleet_mutation_claims'";
        let existing = tx.query_row_typed(load_sql, &[])?;
        if schema_version >= 50 && existing.is_none() {
            return Err(invalid_claim(
                "schema v50 is missing fleet mutation execution authority; refusing to recreate it",
            ));
        }
        if existing.is_none() {
            tx.execute_batch(FLEET_MUTATION_CLAIMS_SCHEMA_SQL)?;
        }
        let actual = tx
            .query_row_typed(load_sql, &[])?
            .ok_or_else(|| invalid_claim("fleet mutation claim schema is missing"))?;
        let actual = RowReader::new(&actual).string(0)?;
        if super::migrations::compact_schema_sql(&actual)
            != super::migrations::compact_schema_sql(FLEET_MUTATION_CLAIMS_SCHEMA_SQL)
        {
            return Err(invalid_claim(
                "fleet mutation claim schema has non-canonical execution authority",
            ));
        }
        Ok(())
    })
}

/// Insert a new fleet mutation receipt. A duplicate idempotency key surfaces as
/// a backend UNIQUE constraint error; callers should re-read the existing row to
/// distinguish replay races from real storage failure.
pub fn insert_fleet_mutation_receipt(
    backend: &dyn StorageBackend,
    record: &FleetMutationReceiptRecord,
) -> Result<(), FleetMutationReceiptsSqlError> {
    validate_text_key("idempotency_key", &record.idempotency_key)?;
    validate_text_key("payload_fingerprint", &record.payload_fingerprint)?;
    validate_text_key("action", &record.action)?;
    validate_text_key("plan_id", &record.plan_id)?;

    let receipt_json = serde_json::to_string(&record.receipt).map_err(|err| {
        FleetMutationReceiptsSqlError::Decode {
            column: "receipt_json",
            msg: err.to_string(),
        }
    })?;
    let params = [
        ToSqlValue::Text(record.idempotency_key.as_str()),
        ToSqlValue::Text(record.payload_fingerprint.as_str()),
        ToSqlValue::Text(record.action.as_str()),
        ToSqlValue::Text(record.plan_id.as_str()),
        ToSqlValue::Integer(i64::from(record.dry_run)),
        ToSqlValue::OwnedText(receipt_json),
        ToSqlValue::Integer(record.recorded_at_ms),
    ];

    execute_typed(
        backend,
        "INSERT INTO fleet_mutation_receipts \
         (idempotency_key, payload_fingerprint, action, plan_id, dry_run, receipt_json, \
          recorded_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        &params,
    )?;
    Ok(())
}

/// Fetch the durable receipt for an idempotency key.
pub fn get_fleet_mutation_receipt(
    backend: &dyn StorageBackend,
    idempotency_key: &str,
) -> Result<Option<FleetMutationReceiptRecord>, FleetMutationReceiptsSqlError> {
    validate_text_key("idempotency_key", idempotency_key)?;
    let row = backend.query_row_typed(
        "SELECT idempotency_key, payload_fingerprint, action, plan_id, dry_run, receipt_json, \
                recorded_at_ms \
         FROM fleet_mutation_receipts \
         WHERE idempotency_key = ?1",
        &[ToSqlValue::Text(idempotency_key)],
    )?;
    match row {
        None => Ok(None),
        Some(row) => Ok(Some(decode_receipt_row(&row)?)),
    }
}

// CASE expressions bound allocation before the backend materializes TEXT.
// Even a damaged/preexisting table cannot make a lookup load an unbounded
// receipt, response, or authority field into the process.
const SELECT_CLAIM_SQL: &str = "
SELECT
    CASE WHEN length(CAST(idempotency_key AS BLOB)) <= 512 THEN idempotency_key END,
    CASE WHEN length(CAST(request_sha256 AS BLOB)) = 64 THEN request_sha256 END,
    CASE WHEN length(CAST(action AS BLOB)) <= 512 THEN action END,
    CASE WHEN length(CAST(payload_fingerprint AS BLOB)) <= 512 THEN payload_fingerprint END,
    CASE WHEN length(CAST(plan_id AS BLOB)) <= 512 THEN plan_id END,
    CASE WHEN length(CAST(owner_nonce AS BLOB)) = 64 THEN owner_nonce END,
    CASE WHEN length(CAST(state AS BLOB)) <= 16 THEN state END,
    CASE WHEN length(CAST(response_data_json AS BLOB)) <= 1048576 THEN response_data_json END,
    response_data_json IS NOT NULL,
    coalesce(length(CAST(response_data_json AS BLOB)), 0),
    CASE WHEN typeof(created_at_ms) = 'integer' THEN created_at_ms END,
    CASE WHEN typeof(updated_at_ms) = 'integer' THEN updated_at_ms END
FROM fleet_mutation_claims
WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY";

const SELECT_BOUNDED_RECEIPT_SQL: &str = "
SELECT
    CASE WHEN length(CAST(idempotency_key AS BLOB)) <= 512 THEN idempotency_key END,
    CASE WHEN length(CAST(payload_fingerprint AS BLOB)) <= 512 THEN payload_fingerprint END,
    CASE WHEN length(CAST(action AS BLOB)) <= 512 THEN action END,
    CASE WHEN length(CAST(plan_id AS BLOB)) <= 512 THEN plan_id END,
    CASE WHEN typeof(dry_run) = 'integer' AND dry_run IN (0, 1) THEN dry_run END,
    CASE WHEN length(CAST(receipt_json AS BLOB)) <= 1048576 THEN receipt_json END,
    CASE WHEN typeof(recorded_at_ms) = 'integer' THEN recorded_at_ms END
FROM fleet_mutation_receipts
WHERE idempotency_key COLLATE BINARY = ?1 COLLATE BINARY";

struct FleetMutationClaimHeader {
    idempotency_key: String,
    request_sha256: String,
    action: String,
    payload_fingerprint: String,
    plan_id: String,
    owner_nonce: String,
    state: String,
    response_data: Option<serde_json::Value>,
}

impl FleetMutationClaimHeader {
    fn matches_binding(&self, binding: &FleetMutationRequestBinding) -> bool {
        self.idempotency_key == binding.idempotency_key()
            && self.request_sha256 == binding.request_sha256()
            && self.action == binding.action()
    }

    fn matches_token(&self, token: &FleetMutationClaimToken) -> bool {
        self.idempotency_key == token.idempotency_key
            && self.request_sha256 == token.request_sha256
            && self.owner_nonce == token.owner_nonce
    }
}

fn invalid_claim(message: &str) -> FleetMutationReceiptsSqlError {
    FleetMutationReceiptsSqlError::Invalid(message.to_string())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_binding(
    binding: &FleetMutationRequestBinding,
) -> Result<(), FleetMutationReceiptsSqlError> {
    if !binding.is_valid() {
        return Err(invalid_claim("invalid fleet request binding"));
    }
    Ok(())
}

fn validate_token_binding(
    binding: &FleetMutationRequestBinding,
    token: &FleetMutationClaimToken,
) -> Result<(), FleetMutationReceiptsSqlError> {
    validate_binding(binding)?;
    if token.idempotency_key != binding.idempotency_key()
        || token.request_sha256 != binding.request_sha256()
        || !valid_digest(&token.owner_nonce)
    {
        return Err(invalid_claim(
            "fleet claim token does not match the request",
        ));
    }
    Ok(())
}

fn fresh_owner_nonce() -> Result<String, FleetMutationReceiptsSqlError> {
    let mut bytes = [0_u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| invalid_claim("OS entropy unavailable for fleet claim ownership"))?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(invalid_claim("invalid entropy for fleet claim ownership"));
    }
    let alphabet = b"0123456789abcdef";
    Ok(bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(alphabet[usize::from(byte >> 4)]),
                char::from(alphabet[usize::from(byte & 15)]),
            ]
        })
        .collect())
}

fn run_claim_transaction<T>(
    backend: &dyn StorageBackend,
    operation: impl FnOnce(&mut dyn StorageTransaction) -> Result<T, FleetMutationReceiptsSqlError>,
) -> Result<T, FleetMutationReceiptsSqlError> {
    let mut operation = Some(operation);
    let mut output = None;
    backend.with_transaction_dyn(&mut |tx| {
        let operation = operation
            .take()
            .ok_or(BackendError::TransactionCallbackInvokedMoreThanOnce)?;
        output = Some(operation(tx).map_err(|error| BackendError::Other(error.to_string()))?);
        Ok(())
    })?;
    output.ok_or_else(|| invalid_claim("fleet claim transaction produced no result"))
}

fn load_claim(
    tx: &mut dyn StorageTransaction,
    key: &str,
) -> Result<Option<FleetMutationClaimHeader>, FleetMutationReceiptsSqlError> {
    let Some(row) = tx.query_row_typed(SELECT_CLAIM_SQL, &[ToSqlValue::Text(key)])? else {
        return Ok(None);
    };
    let reader = RowReader::new(&row);
    let idempotency_key = reader.string(0)?;
    let request_sha256 = reader.string(1)?;
    let action = reader.string(2)?;
    let payload_fingerprint = reader.string(3)?;
    let plan_id = reader.string(4)?;
    let owner_nonce = reader.string(5)?;
    let state = reader.string(6)?;
    for (field, value) in [
        ("idempotency_key", idempotency_key.as_str()),
        ("action", action.as_str()),
        ("payload_fingerprint", payload_fingerprint.as_str()),
        ("plan_id", plan_id.as_str()),
    ] {
        validate_text_key(field, value)?;
    }
    let created_at_ms = reader.i64(10)?;
    let updated_at_ms = reader.i64(11)?;
    if idempotency_key != key
        || !valid_digest(&request_sha256)
        || !valid_digest(&owner_nonce)
        || !matches!(state.as_str(), "claimed" | "in_doubt" | "completed")
        || created_at_ms < 0
        || updated_at_ms < created_at_ms
    {
        return Err(invalid_claim("persisted fleet claim authority is invalid"));
    }
    let response_exists = reader.bool(8)?;
    let response_bytes = reader.i64(9)?;
    if !(0..=MAX_RESPONSE_DATA_BYTES as i64).contains(&response_bytes)
        || (response_exists && state != "completed")
    {
        return Err(invalid_claim(
            "persisted fleet response exceeds its authority bounds",
        ));
    }
    let response_data = if response_exists {
        Some(serde_json::from_str(&reader.string(7)?).map_err(|err| {
            FleetMutationReceiptsSqlError::Decode {
                column: "response_data_json",
                msg: err.to_string(),
            }
        })?)
    } else {
        None
    };
    Ok(Some(FleetMutationClaimHeader {
        idempotency_key,
        request_sha256,
        action,
        payload_fingerprint,
        plan_id,
        owner_nonce,
        state,
        response_data,
    }))
}

fn validate_claimed_receipt(
    record: &FleetMutationReceiptRecord,
) -> Result<(), FleetMutationReceiptsSqlError> {
    for (field, value) in [
        ("idempotency_key", record.idempotency_key.as_str()),
        ("payload_fingerprint", record.payload_fingerprint.as_str()),
        ("action", record.action.as_str()),
        ("plan_id", record.plan_id.as_str()),
    ] {
        validate_text_key(field, value)?;
    }
    if record.dry_run
        || record.receipt.dry_run
        || record.receipt.idempotent_replay
        || record.recorded_at_ms < 0
        || record.idempotency_key != record.receipt.idempotency_key.as_str()
        || record.payload_fingerprint != record.receipt.payload_fingerprint.as_str()
        || record.plan_id != record.receipt.plan_id
        || record.receipt.idempotency_key.action() != "fleet_mutation_plan"
        || record.receipt.payload_fingerprint.action() != "fleet_mutation_plan_payload"
        || record.receipt.status == crate::fleet_mutation::FleetMutationReceiptStatus::DryRun
    {
        return Err(invalid_claim(
            "fleet receipt metadata does not match its canonical result",
        ));
    }
    Ok(())
}

fn load_completed_receipt(
    tx: &mut dyn StorageTransaction,
    header: FleetMutationClaimHeader,
) -> Result<FleetMutationReceiptRecord, FleetMutationReceiptsSqlError> {
    let row = tx
        .query_row_typed(
            SELECT_BOUNDED_RECEIPT_SQL,
            &[ToSqlValue::Text(&header.idempotency_key)],
        )?
        .ok_or_else(|| invalid_claim("completed fleet claim has no durable receipt"))?;
    let mut record = decode_receipt_row(&row)?;
    validate_claimed_receipt(&record)?;
    if record.idempotency_key != header.idempotency_key
        || record.action != header.action
        || record.payload_fingerprint != header.payload_fingerprint
        || record.plan_id != header.plan_id
    {
        return Err(invalid_claim(
            "durable fleet receipt does not match its original claim",
        ));
    }
    record.response_data = header.response_data;
    Ok(record)
}

fn lookup_request_in_transaction(
    tx: &mut dyn StorageTransaction,
    binding: &FleetMutationRequestBinding,
) -> Result<Option<FleetMutationClaimOutcome>, FleetMutationReceiptsSqlError> {
    if let Some(header) = load_claim(tx, binding.idempotency_key())? {
        if !header.matches_binding(binding) {
            return Ok(Some(FleetMutationClaimOutcome::Conflict));
        }
        return Ok(Some(if header.state == "completed" {
            FleetMutationClaimOutcome::Completed(load_completed_receipt(tx, header)?)
        } else {
            FleetMutationClaimOutcome::InDoubt
        }));
    }
    Ok(tx
        .query_row_typed(
            "SELECT 1 FROM fleet_mutation_receipts WHERE idempotency_key = ?1",
            &[ToSqlValue::Text(binding.idempotency_key())],
        )?
        .map(|_| FleetMutationClaimOutcome::LegacyUnbound))
}

/// Inspect the original request before constructing a new inventory-based plan.
/// This read-only snapshot never creates or transfers execution authority.
pub fn lookup_fleet_mutation_request(
    backend: &dyn StorageBackend,
    binding: &FleetMutationRequestBinding,
) -> Result<Option<FleetMutationClaimOutcome>, FleetMutationReceiptsSqlError> {
    validate_binding(binding)?;
    run_claim_transaction(backend, |tx| lookup_request_in_transaction(tx, binding))
}

fn enforce_claim_capacity(
    tx: &mut dyn StorageTransaction,
) -> Result<(), FleetMutationReceiptsSqlError> {
    // Reserve both maximum result blobs before admitting effects. Pending and
    // in-doubt ownership is never pruned to make room for another execution.
    let row = tx
        .query_row_typed(
            "SELECT count(*), coalesce(sum(?1 + CASE WHEN c.state = 'completed' THEN \
                 coalesce(length(CAST(r.receipt_json AS BLOB)), 0) + \
                 coalesce(length(CAST(c.response_data_json AS BLOB)), 0) \
                 ELSE ?2 END), 0) \
             FROM fleet_mutation_claims c \
             LEFT JOIN fleet_mutation_receipts r ON r.idempotency_key = c.idempotency_key",
            &[
                ToSqlValue::Integer(CLAIM_METADATA_RESERVE_BYTES),
                ToSqlValue::Integer((MAX_NEW_RECEIPT_BYTES + MAX_RESPONSE_DATA_BYTES) as i64),
            ],
        )?
        .ok_or_else(|| invalid_claim("fleet claim capacity query produced no result"))?;
    let reader = RowReader::new(&row);
    let records = reader.i64(0)?;
    let logical_bytes = reader.i64(1)?;
    if !(0..=MAX_CLAIM_RECORDS).contains(&records)
        || !(0..=MAX_CLAIM_LOGICAL_BYTES).contains(&logical_bytes)
    {
        return Err(invalid_claim(
            "durable fleet claim capacity exhausted; existing claims retained",
        ));
    }
    Ok(())
}

/// Reserve the sole executor before effects. A completed matching request
/// replays its original plan even if the newly derived inventory plan differs.
pub fn claim_fleet_mutation_request(
    backend: &dyn StorageBackend,
    binding: &FleetMutationRequestBinding,
    plan: &FleetMutationPlan,
    now_ms: i64,
) -> Result<FleetMutationClaimOutcome, FleetMutationReceiptsSqlError> {
    validate_binding(binding)?;
    validate_text_key("payload_fingerprint", plan.payload_fingerprint.as_str())?;
    validate_text_key("plan_id", &plan.plan_id)?;
    if plan.dry_run || plan.idempotency_key.as_str() != binding.idempotency_key() || now_ms < 0 {
        return Err(invalid_claim(
            "fleet claim requires a matching non-dry-run plan and valid time",
        ));
    }
    let token = FleetMutationClaimToken {
        idempotency_key: binding.idempotency_key().to_string(),
        request_sha256: binding.request_sha256().to_string(),
        owner_nonce: fresh_owner_nonce()?,
    };
    run_claim_transaction(backend, |tx| {
        // The transaction backend begins DEFERRED. A write must precede any
        // read, and RETURNING (not nonce equality) identifies the winner.
        let inserted = tx
            .query_row_typed(
                "INSERT INTO fleet_mutation_claims \
                 (idempotency_key, request_sha256, action, payload_fingerprint, plan_id, \
                  owner_nonce, state, response_data_json, created_at_ms, updated_at_ms) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, 'claimed', NULL, ?7, ?7 \
                 WHERE NOT EXISTS (SELECT 1 FROM fleet_mutation_receipts WHERE idempotency_key = ?1) \
                 ON CONFLICT(idempotency_key) DO NOTHING RETURNING idempotency_key",
                &[
                    ToSqlValue::Text(binding.idempotency_key()),
                    ToSqlValue::Text(binding.request_sha256()),
                    ToSqlValue::Text(binding.action()),
                    ToSqlValue::Text(plan.payload_fingerprint.as_str()),
                    ToSqlValue::Text(&plan.plan_id),
                    ToSqlValue::Text(&token.owner_nonce),
                    ToSqlValue::Integer(now_ms),
                ],
            )?
            .is_some();
        if inserted {
            let header = load_claim(tx, binding.idempotency_key())?
                .ok_or_else(|| invalid_claim("new fleet claim disappeared before commit"))?;
            if !header.matches_binding(binding)
                || !header.matches_token(&token)
                || header.state != "claimed"
                || header.payload_fingerprint != plan.payload_fingerprint.as_str()
                || header.plan_id != plan.plan_id
            {
                return Err(invalid_claim("new fleet claim failed its ownership check"));
            }
            enforce_claim_capacity(tx)?;
            Ok(FleetMutationClaimOutcome::Claimed(token))
        } else {
            lookup_request_in_transaction(tx, binding)?.ok_or_else(|| {
                invalid_claim(
                    "fleet claim insert produced neither ownership nor an existing request",
                )
            })
        }
    })
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "fleet result JSON exceeds its byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json(
    value: &impl serde::Serialize,
    column: &'static str,
    limit: usize,
) -> Result<String, FleetMutationReceiptsSqlError> {
    let mut buffer = BoundedJsonBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).map_err(|err| {
        FleetMutationReceiptsSqlError::Decode {
            column,
            msg: err.to_string(),
        }
    })?;
    String::from_utf8(buffer.bytes)
        .map_err(|_| invalid_claim("fleet result JSON was not valid UTF-8"))
}

/// Atomically finalize the original owner's receipt and response. Retrying the
/// exact completion is safe; changing any result or using a stale token fails.
pub fn complete_fleet_mutation_request(
    backend: &dyn StorageBackend,
    binding: &FleetMutationRequestBinding,
    token: &FleetMutationClaimToken,
    record: &FleetMutationReceiptRecord,
) -> Result<(), FleetMutationReceiptsSqlError> {
    validate_token_binding(binding, token)?;
    validate_claimed_receipt(record)?;
    if record.idempotency_key != binding.idempotency_key() || record.action != binding.action() {
        return Err(invalid_claim(
            "fleet receipt does not match its request binding",
        ));
    }
    let receipt_json = bounded_json(&record.receipt, "receipt_json", MAX_NEW_RECEIPT_BYTES)?;
    let response_json = record
        .response_data
        .as_ref()
        .map(|value| bounded_json(value, "response_data_json", MAX_RESPONSE_DATA_BYTES))
        .transpose()?;
    run_claim_transaction(backend, |tx| {
        // Acquire write authority without changing the logical state. A failed
        // receipt insert or final transition rolls back this whole transaction.
        tx.query_row_typed(
            "UPDATE fleet_mutation_claims SET updated_at_ms = max(updated_at_ms, ?5) \
             WHERE idempotency_key = ?1 AND request_sha256 = ?2 AND action = ?3 \
                 AND owner_nonce = ?4 AND state = 'claimed'",
            &[
                ToSqlValue::Text(binding.idempotency_key()),
                ToSqlValue::Text(binding.request_sha256()),
                ToSqlValue::Text(binding.action()),
                ToSqlValue::Text(&token.owner_nonce),
                ToSqlValue::Integer(record.recorded_at_ms),
            ],
        )?;
        let header = load_claim(tx, binding.idempotency_key())?
            .ok_or_else(|| invalid_claim("fleet completion has no original claim"))?;
        if !header.matches_binding(binding) || !header.matches_token(token) {
            return Err(invalid_claim(
                "fleet completion lost its original ownership",
            ));
        }
        if header.state == "completed" {
            return if load_completed_receipt(tx, header)? == *record {
                Ok(())
            } else {
                Err(invalid_claim(
                    "fleet completion differs from the durable result",
                ))
            };
        }
        if header.state != "claimed"
            || header.payload_fingerprint != record.payload_fingerprint
            || header.plan_id != record.plan_id
        {
            return Err(invalid_claim(
                "fleet completion is stale or does not match its original plan",
            ));
        }
        tx.query_row_typed(
            "INSERT INTO fleet_mutation_receipts \
             (idempotency_key, payload_fingerprint, action, plan_id, dry_run, receipt_json, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            &[
                ToSqlValue::Text(&record.idempotency_key),
                ToSqlValue::Text(&record.payload_fingerprint),
                ToSqlValue::Text(&record.action),
                ToSqlValue::Text(&record.plan_id),
                ToSqlValue::Text(&receipt_json),
                ToSqlValue::Integer(record.recorded_at_ms),
            ],
        )?;
        let finalized = tx.query_row_typed(
            "UPDATE fleet_mutation_claims SET state = 'completed', response_data_json = ?5 \
             WHERE idempotency_key = ?1 AND request_sha256 = ?2 AND action = ?3 \
                 AND owner_nonce = ?4 AND state = 'claimed' RETURNING idempotency_key",
            &[
                ToSqlValue::Text(binding.idempotency_key()),
                ToSqlValue::Text(binding.request_sha256()),
                ToSqlValue::Text(binding.action()),
                ToSqlValue::Text(&token.owner_nonce),
                response_json
                    .as_deref()
                    .map_or(ToSqlValue::Null, ToSqlValue::Text),
            ],
        )?;
        if finalized.is_none() {
            return Err(invalid_claim(
                "fleet completion lost ownership during finalization",
            ));
        }
        Ok(())
    })
}

/// Revoke a live owner's completion authority after an indeterminate result.
/// This is a durable fence, not a release: no caller may execute this key again.
pub fn mark_fleet_mutation_in_doubt(
    backend: &dyn StorageBackend,
    binding: &FleetMutationRequestBinding,
    token: &FleetMutationClaimToken,
    now_ms: i64,
) -> Result<(), FleetMutationReceiptsSqlError> {
    validate_token_binding(binding, token)?;
    if now_ms < 0 {
        return Err(invalid_claim(
            "fleet in-doubt transition requires a valid time",
        ));
    }
    run_claim_transaction(backend, |tx| {
        let changed = tx.query_row_typed(
            "UPDATE fleet_mutation_claims SET state = 'in_doubt', updated_at_ms = max(updated_at_ms, ?5) \
             WHERE idempotency_key = ?1 AND request_sha256 = ?2 AND action = ?3 \
                 AND owner_nonce = ?4 AND state IN ('claimed', 'in_doubt') RETURNING idempotency_key",
            &[
                ToSqlValue::Text(binding.idempotency_key()),
                ToSqlValue::Text(binding.request_sha256()),
                ToSqlValue::Text(binding.action()),
                ToSqlValue::Text(&token.owner_nonce),
                ToSqlValue::Integer(now_ms),
            ],
        )?;
        if changed.is_none() {
            return Err(invalid_claim(
                "fleet in-doubt transition lost its original ownership",
            ));
        }
        let header = load_claim(tx, binding.idempotency_key())?
            .ok_or_else(|| invalid_claim("fleet in-doubt claim disappeared"))?;
        if !header.matches_binding(binding)
            || !header.matches_token(token)
            || header.state != "in_doubt"
        {
            return Err(invalid_claim(
                "fleet in-doubt transition failed its ownership check",
            ));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_mutation::{
        FleetMutationAction, FleetMutationPlan, FleetMutationPolicyDecision,
        FleetMutationReceiptStatus, FleetMutationStep, FleetMutationStepStatus,
    };

    fn fresh_db() -> RusqliteBackend {
        let backend = RusqliteBackend::open(
            ":memory:",
            &OpenConfig {
                wal_mode: false,
                ..OpenConfig::default()
            },
        )
        .unwrap();
        ensure_fleet_mutation_receipts_schema(&backend).unwrap();
        backend
    }

    fn sample_plan() -> FleetMutationPlan {
        let step = FleetMutationStep {
            step_id: "spawn-1".to_string(),
            action: FleetMutationAction::SpawnAgent {
                profile_name: "codex".to_string(),
                program: "codex".to_string(),
                cwd: None,
                domain: None,
                env: Default::default(),
            },
            policy: FleetMutationPolicyDecision::Allow,
            compensation: None,
        };
        FleetMutationPlan::with_client_key("fleet-scale:codex:0->1", false, vec![step], "client-a")
    }

    fn sample_binding() -> FleetMutationRequestBinding {
        FleetMutationRequestBinding::new(
            "client-a",
            "scale",
            &serde_json::json!({"provider": "codex", "count": 1}),
        )
        .unwrap()
    }

    fn sample_receipt() -> FleetMutationReceipt {
        let plan = sample_plan();
        let mut receipt = crate::fleet_mutation::FleetMutationLedger::new()
            .execute_plan(&plan, &mut NoopExecutor)
            .unwrap();
        receipt.status = FleetMutationReceiptStatus::Succeeded;
        receipt.steps[0].status = FleetMutationStepStatus::Succeeded;
        receipt
    }

    fn claim_owner(
        db: &dyn StorageBackend,
        binding: &FleetMutationRequestBinding,
        plan: &FleetMutationPlan,
    ) -> FleetMutationClaimToken {
        match claim_fleet_mutation_request(db, binding, plan, 10).unwrap() {
            FleetMutationClaimOutcome::Claimed(token) => token,
            other => panic!("expected sole execution owner, got {other:?}"),
        }
    }

    fn open_file_db(path: &std::path::Path) -> RusqliteBackend {
        let backend =
            RusqliteBackend::open(path.to_str().unwrap(), &OpenConfig::default()).unwrap();
        backend
            .set_busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        ensure_fleet_mutation_receipts_schema(&backend).unwrap();
        backend
    }

    struct NoopExecutor;

    impl crate::fleet_mutation::FleetMutationExecutor for NoopExecutor {
        fn execute_step(
            &mut self,
            _step: &FleetMutationStep,
        ) -> Result<
            crate::fleet_mutation::FleetMutationStepOutput,
            crate::fleet_mutation::FleetMutationExecutionError,
        > {
            Ok(crate::fleet_mutation::FleetMutationStepOutput::default())
        }

        fn compensate_step(
            &mut self,
            _original: &FleetMutationStep,
            _compensation: &FleetMutationAction,
        ) -> Result<
            crate::fleet_mutation::FleetMutationStepOutput,
            crate::fleet_mutation::FleetMutationExecutionError,
        > {
            Ok(crate::fleet_mutation::FleetMutationStepOutput::default())
        }
    }

    #[test]
    fn insert_then_get_round_trips_receipt() {
        let db = fresh_db();
        let receipt = sample_receipt();
        let record = FleetMutationReceiptRecord::from_receipt("scale", receipt.clone(), 42);

        insert_fleet_mutation_receipt(&db, &record).unwrap();
        let fetched = get_fleet_mutation_receipt(&db, receipt.idempotency_key.as_str())
            .unwrap()
            .unwrap();

        assert_eq!(fetched.action, "scale");
        assert_eq!(fetched.recorded_at_ms, 42);
        assert_eq!(fetched.receipt, receipt);
    }

    #[test]
    fn get_returns_none_for_unknown_key() {
        let db = fresh_db();
        let fetched = get_fleet_mutation_receipt(&db, "missing-key").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn duplicate_insert_returns_unique_constraint_error() {
        let db = fresh_db();
        let receipt = sample_receipt();
        let record = FleetMutationReceiptRecord::from_receipt("scale", receipt, 42);
        insert_fleet_mutation_receipt(&db, &record).unwrap();

        let err = insert_fleet_mutation_receipt(&db, &record).unwrap_err();
        match err {
            FleetMutationReceiptsSqlError::Backend(BackendError::Query(msg)) => {
                assert!(msg.contains("UNIQUE"));
            }
            other => panic!("expected UNIQUE backend error, got {other:?}"),
        }
    }

    #[test]
    fn insert_rejects_empty_key() {
        let db = fresh_db();
        let receipt = sample_receipt();
        let mut record = FleetMutationReceiptRecord::from_receipt("scale", receipt, 42);
        record.idempotency_key.clear();

        let err = insert_fleet_mutation_receipt(&db, &record).unwrap_err();
        assert!(matches!(err, FleetMutationReceiptsSqlError::Invalid(_)));
    }

    #[test]
    fn direct_initialization_supports_fresh_and_v49_databases_without_rebinding_legacy() {
        let fresh = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        ensure_fleet_mutation_receipts_schema(&fresh).unwrap();
        assert_eq!(fresh.user_version().unwrap(), 0);
        let _owner = claim_owner(&fresh, &sample_binding(), &sample_plan());
        ensure_fleet_mutation_receipts_schema(&fresh).unwrap();
        assert_eq!(
            lookup_fleet_mutation_request(&fresh, &sample_binding()).unwrap(),
            Some(FleetMutationClaimOutcome::InDoubt)
        );

        let legacy = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
        legacy
            .execute_batch(FLEET_MUTATION_RECEIPTS_SCHEMA_SQL)
            .unwrap();
        legacy.set_user_version(49).unwrap();
        let record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        insert_fleet_mutation_receipt(&legacy, &record).unwrap();
        ensure_fleet_mutation_receipts_schema(&legacy).unwrap();
        assert_eq!(legacy.user_version().unwrap(), 49);
        assert_eq!(
            get_fleet_mutation_receipt(&legacy, "client-a").unwrap(),
            Some(record)
        );
        assert_eq!(
            lookup_fleet_mutation_request(&legacy, &sample_binding()).unwrap(),
            Some(FleetMutationClaimOutcome::LegacyUnbound)
        );
    }

    #[test]
    fn direct_initialization_refuses_lost_or_drifted_current_execution_authority() {
        for version in [50, 51] {
            let missing = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
            missing
                .execute_batch(FLEET_MUTATION_RECEIPTS_SCHEMA_SQL)
                .unwrap();
            missing.set_user_version(version).unwrap();
            let record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
            insert_fleet_mutation_receipt(&missing, &record).unwrap();
            assert!(ensure_fleet_mutation_receipts_schema(&missing).is_err());
            assert_eq!(
                missing
                    .query_scalar(
                        "SELECT count(*) FROM sqlite_schema WHERE name = 'fleet_mutation_claims'"
                    )
                    .unwrap(),
                Some("0".to_string())
            );
            assert_eq!(
                get_fleet_mutation_receipt(&missing, "client-a").unwrap(),
                Some(record)
            );
        }

        for version in [0, 49, 50] {
            let drifted = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
            drifted
                .execute_batch(FLEET_MUTATION_RECEIPTS_SCHEMA_SQL)
                .unwrap();
            drifted
                .execute_batch(
                    &FLEET_MUTATION_CLAIMS_SCHEMA_SQL
                        .replace("BETWEEN 1 AND 512", "BETWEEN 1 AND 513"),
                )
                .unwrap();
            drifted.set_user_version(version).unwrap();
            let before = drifted
                .query_scalar("SELECT sql FROM sqlite_schema WHERE name = 'fleet_mutation_claims'")
                .unwrap();
            assert!(ensure_fleet_mutation_receipts_schema(&drifted).is_err());
            assert_eq!(
                drifted
                    .query_scalar(
                        "SELECT sql FROM sqlite_schema WHERE name = 'fleet_mutation_claims'"
                    )
                    .unwrap(),
                before
            );
        }
    }

    #[test]
    fn concurrent_sqlite_connections_admit_exactly_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.sqlite3");
        let first = open_file_db(&path);
        let second = open_file_db(&path);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [first, second]
            .into_iter()
            .map(|db| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    claim_fleet_mutation_request(&db, &sample_binding(), &sample_plan(), 10)
                        .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, FleetMutationClaimOutcome::Claimed(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, FleetMutationClaimOutcome::InDoubt))
                .count(),
            1
        );
        let reopened = open_file_db(&path);
        assert_eq!(
            reopened
                .query_scalar("SELECT count(*) FROM fleet_mutation_claims")
                .unwrap(),
            Some("1".to_string())
        );
    }

    #[test]
    fn interruption_and_clock_changes_never_regrant_execution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.sqlite3");
        {
            let db = open_file_db(&path);
            let _lost_owner = claim_owner(&db, &sample_binding(), &sample_plan());
        }
        for now_ms in [0, 10, 1_000_000, i64::MAX] {
            let db = open_file_db(&path);
            assert_eq!(
                claim_fleet_mutation_request(&db, &sample_binding(), &sample_plan(), now_ms)
                    .unwrap(),
                FleetMutationClaimOutcome::InDoubt
            );
            assert_eq!(
                lookup_fleet_mutation_request(&db, &sample_binding()).unwrap(),
                Some(FleetMutationClaimOutcome::InDoubt)
            );
            assert!(
                get_fleet_mutation_receipt(&db, "client-a")
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn completion_preserves_original_plan_and_partial_response_on_replay() {
        let db = fresh_db();
        let binding = sample_binding();
        assert_eq!(lookup_fleet_mutation_request(&db, &binding).unwrap(), None);
        let token = claim_owner(&db, &binding, &sample_plan());
        let mut record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        record.response_data = Some(serde_json::json!({
            "status": "scale_down_partial_plan_created",
            "remaining_delta": -2,
            "current_count": 4,
        }));
        complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();

        let changed_inventory_plan = FleetMutationPlan::with_client_key(
            "fleet-scale:codex:9->1",
            false,
            Vec::new(),
            "client-a",
        );
        assert_eq!(
            claim_fleet_mutation_request(&db, &binding, &changed_inventory_plan, 99).unwrap(),
            FleetMutationClaimOutcome::Completed(record.clone())
        );
        assert_eq!(
            lookup_fleet_mutation_request(&db, &binding).unwrap(),
            Some(FleetMutationClaimOutcome::Completed(record))
        );
        for (action, request) in [
            (
                "scale",
                serde_json::json!({"provider": "codex", "count": 2}),
            ),
            (
                "scale",
                serde_json::json!({"provider": "claude", "count": 1}),
            ),
            ("rebalance", serde_json::json!({"strategy": "balanced"})),
        ] {
            let changed = FleetMutationRequestBinding::new("client-a", action, &request).unwrap();
            assert_eq!(
                lookup_fleet_mutation_request(&db, &changed).unwrap(),
                Some(FleetMutationClaimOutcome::Conflict)
            );
            assert_eq!(
                claim_fleet_mutation_request(&db, &changed, &changed_inventory_plan, 99).unwrap(),
                FleetMutationClaimOutcome::Conflict
            );
        }
    }

    #[test]
    fn completion_is_atomic_and_exactly_repeatable_after_storage_failure() {
        for trigger in [
            "CREATE TRIGGER reject_completion BEFORE INSERT ON fleet_mutation_receipts \
             BEGIN SELECT RAISE(ABORT, 'receipt insert failed'); END",
            "CREATE TRIGGER reject_completion BEFORE UPDATE OF state ON fleet_mutation_claims \
             WHEN NEW.state = 'completed' \
             BEGIN SELECT RAISE(ABORT, 'claim finalization failed'); END",
        ] {
            let db = fresh_db();
            let binding = sample_binding();
            let token = claim_owner(&db, &binding, &sample_plan());
            let mut record =
                FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
            record.response_data = Some(serde_json::json!({"remaining_delta": 0}));
            db.execute_batch(trigger).unwrap();

            assert!(complete_fleet_mutation_request(&db, &binding, &token, &record).is_err());
            assert!(
                get_fleet_mutation_receipt(&db, "client-a")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                db.query_scalar("SELECT state FROM fleet_mutation_claims")
                    .unwrap(),
                Some("claimed".to_string())
            );
            assert_eq!(
                claim_fleet_mutation_request(&db, &binding, &sample_plan(), 100).unwrap(),
                FleetMutationClaimOutcome::InDoubt
            );

            db.execute_batch("DROP TRIGGER reject_completion").unwrap();
            complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();
            complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();
            let mut changed_response = record.clone();
            changed_response.response_data = Some(serde_json::json!({"remaining_delta": 2}));
            assert!(
                complete_fleet_mutation_request(&db, &binding, &token, &changed_response).is_err()
            );
            let mut changed_receipt = record.clone();
            changed_receipt.receipt.status = FleetMutationReceiptStatus::Failed;
            assert!(
                complete_fleet_mutation_request(&db, &binding, &token, &changed_receipt).is_err()
            );
            assert_eq!(
                lookup_fleet_mutation_request(&db, &binding).unwrap(),
                Some(FleetMutationClaimOutcome::Completed(record))
            );
        }
    }

    #[test]
    fn stale_tokens_and_inconsistent_metadata_cannot_complete_or_revoke() {
        let db = fresh_db();
        let binding = sample_binding();
        let token = claim_owner(&db, &binding, &sample_plan());
        let record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        let mut stale = token.clone();
        let replacement = if stale.owner_nonce.starts_with('a') {
            "b"
        } else {
            "a"
        };
        stale.owner_nonce.replace_range(..1, replacement);
        assert!(complete_fleet_mutation_request(&db, &binding, &stale, &record).is_err());
        assert!(mark_fleet_mutation_in_doubt(&db, &binding, &stale, 20).is_err());

        let mut mismatched = record.clone();
        mismatched.plan_id = "another-plan".to_string();
        assert!(complete_fleet_mutation_request(&db, &binding, &token, &mismatched).is_err());
        assert!(
            get_fleet_mutation_receipt(&db, "client-a")
                .unwrap()
                .is_none()
        );
        complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();
        assert!(complete_fleet_mutation_request(&db, &binding, &stale, &record).is_err());
    }

    #[test]
    fn explicit_in_doubt_revokes_owner_without_a_clock_based_retry() {
        let db = fresh_db();
        let binding = sample_binding();
        let token = claim_owner(&db, &binding, &sample_plan());
        // Clock rollback must preserve, rather than erase, the effect fence.
        mark_fleet_mutation_in_doubt(&db, &binding, &token, 0).unwrap();
        mark_fleet_mutation_in_doubt(&db, &binding, &token, 0).unwrap();
        let record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        assert!(complete_fleet_mutation_request(&db, &binding, &token, &record).is_err());
        assert_eq!(
            claim_fleet_mutation_request(&db, &binding, &sample_plan(), i64::MAX).unwrap(),
            FleetMutationClaimOutcome::InDoubt
        );
        assert_eq!(
            db.query_scalar("SELECT updated_at_ms FROM fleet_mutation_claims")
                .unwrap(),
            Some("10".to_string())
        );
    }

    #[test]
    fn legacy_receipts_are_preserved_without_adopting_an_unbound_key() {
        let db = fresh_db();
        let record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        insert_fleet_mutation_receipt(&db, &record).unwrap();
        assert_eq!(
            get_fleet_mutation_receipt(&db, "client-a").unwrap(),
            Some(record)
        );
        assert_eq!(
            lookup_fleet_mutation_request(&db, &sample_binding()).unwrap(),
            Some(FleetMutationClaimOutcome::LegacyUnbound)
        );
        assert_eq!(
            claim_fleet_mutation_request(&db, &sample_binding(), &sample_plan(), 100).unwrap(),
            FleetMutationClaimOutcome::LegacyUnbound
        );
        assert_eq!(
            db.query_scalar("SELECT count(*) FROM fleet_mutation_claims")
                .unwrap(),
            Some("0".to_string())
        );
    }

    #[test]
    fn explicit_no_op_is_durable_and_dry_run_cannot_acquire_ownership() {
        let db = fresh_db();
        let binding = sample_binding();
        let dry_plan = FleetMutationPlan::with_client_key("no-op", true, Vec::new(), "client-a");
        assert!(claim_fleet_mutation_request(&db, &binding, &dry_plan, 10).is_err());
        assert_eq!(lookup_fleet_mutation_request(&db, &binding).unwrap(), None);

        let plan = FleetMutationPlan::with_client_key("no-op", false, Vec::new(), "client-a");
        let token = claim_owner(&db, &binding, &plan);
        let receipt = FleetMutationReceipt::no_op(&plan).unwrap();
        let record = FleetMutationReceiptRecord::from_receipt("scale", receipt, 42);
        complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();
        assert_eq!(
            claim_fleet_mutation_request(&db, &binding, &sample_plan(), 100).unwrap(),
            FleetMutationClaimOutcome::Completed(record)
        );
    }

    #[test]
    fn oversized_results_and_corrupt_completed_rows_fail_without_regranting() {
        let db = fresh_db();
        let binding = sample_binding();
        let token = claim_owner(&db, &binding, &sample_plan());
        let mut record = FleetMutationReceiptRecord::from_receipt("scale", sample_receipt(), 42);
        record.response_data = Some(serde_json::Value::String(
            "x".repeat(MAX_RESPONSE_DATA_BYTES),
        ));
        assert!(complete_fleet_mutation_request(&db, &binding, &token, &record).is_err());
        assert!(
            get_fleet_mutation_receipt(&db, "client-a")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            lookup_fleet_mutation_request(&db, &binding).unwrap(),
            Some(FleetMutationClaimOutcome::InDoubt)
        );
        record.response_data = None;
        complete_fleet_mutation_request(&db, &binding, &token, &record).unwrap();
        db.execute("UPDATE fleet_mutation_receipts SET receipt_json = printf('%1048577s', 'x')")
            .unwrap();
        assert!(lookup_fleet_mutation_request(&db, &binding).is_err());
        assert!(claim_fleet_mutation_request(&db, &binding, &sample_plan(), 100).is_err());
        assert_eq!(
            db.query_scalar("SELECT state FROM fleet_mutation_claims")
                .unwrap(),
            Some("completed".to_string())
        );
    }

    #[test]
    fn capacity_failure_rolls_back_new_admission_without_forgetting_old_claims() {
        let db = fresh_db();
        let reservation =
            CLAIM_METADATA_RESERVE_BYTES + (MAX_NEW_RECEIPT_BYTES + MAX_RESPONSE_DATA_BYTES) as i64;
        let admitted = MAX_CLAIM_LOGICAL_BYTES / reservation;
        for index in 0..admitted {
            let key = format!("pending-{index}");
            let binding =
                FleetMutationRequestBinding::new(&key, "scale", &serde_json::json!(index)).unwrap();
            let plan = FleetMutationPlan::with_client_key("no-op", false, Vec::new(), &key);
            claim_owner(&db, &binding, &plan);
        }
        assert!(claim_fleet_mutation_request(&db, &sample_binding(), &sample_plan(), 10).is_err());
        assert_eq!(
            lookup_fleet_mutation_request(&db, &sample_binding()).unwrap(),
            None
        );
        assert_eq!(
            db.query_scalar("SELECT count(*) FROM fleet_mutation_claims")
                .unwrap(),
            Some(admitted.to_string())
        );
        let original =
            FleetMutationRequestBinding::new("pending-0", "scale", &serde_json::json!(0)).unwrap();
        assert_eq!(
            lookup_fleet_mutation_request(&db, &original).unwrap(),
            Some(FleetMutationClaimOutcome::InDoubt)
        );
    }
}
