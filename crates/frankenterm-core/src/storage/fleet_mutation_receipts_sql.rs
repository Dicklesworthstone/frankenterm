//! StorageBackend CRUD primitives for durable fleet mutation receipts.
//!
//! The `ft robot fleet scale` and `ft robot fleet rebalance` commit paths use
//! this table to make non-dry-run receipts survive process restarts. The
//! in-memory [`crate::fleet_mutation::FleetMutationLedger`] still owns plan
//! execution semantics; this module only persists completed receipts and lets
//! callers detect replay or payload conflicts before issuing side effects.
//!
//! Schema lives in storage/migrations.rs at version 27.

use crate::fleet_mutation::FleetMutationReceipt;
use crate::storage_backend_helpers::execute_typed;
use crate::storage_backend_row_helpers::RowReader;
use crate::storage_backend_trait::{BackendError, StorageBackend, ToSqlValue};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetMutationReceiptRecord {
    pub idempotency_key: String,
    pub payload_fingerprint: String,
    pub action: String,
    pub plan_id: String,
    pub dry_run: bool,
    pub receipt: FleetMutationReceipt,
    pub recorded_at_ms: i64,
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
        }
    }
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
    })
}

/// Ensure the durable receipt table exists for direct Robot CLI paths that open
/// the workspace database outside the main storage migration runner.
pub fn ensure_fleet_mutation_receipts_schema(
    backend: &dyn StorageBackend,
) -> Result<(), FleetMutationReceiptsSqlError> {
    backend.execute_batch(FLEET_MUTATION_RECEIPTS_SCHEMA_SQL)?;
    Ok(())
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

    fn sample_receipt() -> FleetMutationReceipt {
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
        let plan = FleetMutationPlan::with_client_key(
            "fleet-scale:codex:0->1",
            false,
            vec![step],
            "client-a",
        );
        let mut receipt = crate::fleet_mutation::FleetMutationLedger::new()
            .execute_plan(&plan, &mut NoopExecutor)
            .unwrap();
        receipt.status = FleetMutationReceiptStatus::Succeeded;
        receipt.steps[0].status = FleetMutationStepStatus::Succeeded;
        receipt
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
}
