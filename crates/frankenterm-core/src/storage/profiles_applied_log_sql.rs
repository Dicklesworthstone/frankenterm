//! SQLite CRUD primitives for the `profiles_applied_log` table
//! (br-ft-4iz0q substrate-pass).
//!
//! Synchronous handlers callable from the storage writer thread
//! (or from tests via in-memory SQLite). The async StorageHandle
//! wrappers + WriteCommand variants are a separate slice — this
//! module ships the actual SQL so a future commit can plug them
//! into the writer-loop dispatch table without re-litigating
//! schema details.
//!
//! Schema lives in storage/migrations.rs at version 26.
//!
//! ## Table semantics
//!
//! Each row records one non-dry-run `RobotProfile.apply` request
//! that the daemon-side handler processed. Subsequent apply
//! requests with the same `content_hash` short-circuit to the
//! recorded `panes_spawned_json` payload per the bead's
//! idempotency contract.
//!
//! `content_hash` is the canonical hex SHA-256 returned by
//! [`crate::robot_profile_handler::compute_apply_content_hash`]
//! over `(profile_name, profile_updated_at_ms, count, env)`.
//! Profile mutations bump `updated_at_ms`, which invalidates
//! prior receipts (the hash diverges) — that's the bead's intent.
//!
//! ## JSON columns
//!
//! `panes_spawned_json` round-trips through `serde_json` against
//! `Vec<u64>`. Insert side uses `serde_json::to_string` which is
//! infallible for owned `Vec<u64>`; read side maps an
//! unparseable value to [`ProfilesAppliedLogSqlError::Decode`]
//! (DB corruption signal).
//!
//! Cross-references:
//! - bead: ft-4iz0q (substrate-pass landed at 6a3653dc2 ships
//!   the ApplyReceipt + compute_apply_content_hash; cc_2's
//!   substrate-pass at 830a4d1ef adds the typed RPC envelope
//!   types).
//! - migration: storage/migrations.rs MIGRATIONS[25] (v26).
//! - substrate types: crates/frankenterm-core/src/robot_profile_handler.rs
//!   (ApplyReceipt struct + compute_apply_content_hash).

use rusqlite::{Connection, OptionalExtension, params};

use crate::robot_profile_handler::ApplyReceipt;

/// Error type for the profiles_applied_log SQL primitives.
#[derive(Debug)]
pub enum ProfilesAppliedLogSqlError {
    /// Underlying SQLite call failed.
    Sqlite(rusqlite::Error),
    /// A JSON-encoded TEXT column (panes_spawned_json) failed to
    /// decode. Carries the column name + serde_json error
    /// message for operator diagnosis. Indicates DB corruption.
    Decode { column: &'static str, msg: String },
    /// Caller-side input validation failed (e.g. content_hash
    /// not 64 hex chars).
    Invalid(String),
}

impl std::fmt::Display for ProfilesAppliedLogSqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "profiles_applied_log SQLite error: {e}"),
            Self::Decode { column, msg } => {
                write!(
                    f,
                    "profiles_applied_log column `{column}` decode failed: {msg}"
                )
            }
            Self::Invalid(msg) => write!(f, "profiles_applied_log invalid input: {msg}"),
        }
    }
}

impl std::error::Error for ProfilesAppliedLogSqlError {}

impl From<rusqlite::Error> for ProfilesAppliedLogSqlError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

/// Insert a new apply receipt. Caller has already computed the
/// content_hash via `compute_apply_content_hash` and verified
/// that the daemon-side handler should write a fresh receipt
/// (i.e. the lookup via [`get_apply_receipt`] returned `None`
/// before the spawn loop ran).
///
/// Returns `Err(ProfilesAppliedLogSqlError::Sqlite)` with a
/// UNIQUE constraint violation if a row with the same
/// content_hash already exists — the daemon-side handler
/// should treat that as a race-loser signal and re-read the
/// existing receipt.
pub fn insert_apply_receipt(
    conn: &Connection,
    receipt: &ApplyReceipt,
) -> Result<(), ProfilesAppliedLogSqlError> {
    if receipt.content_hash.len() != 64
        || !receipt.content_hash.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ProfilesAppliedLogSqlError::Invalid(format!(
            "content_hash must be 64 lowercase-hex chars, got `{}` ({} chars)",
            receipt.content_hash,
            receipt.content_hash.len(),
        )));
    }

    let panes_json =
        serde_json::to_string(&receipt.panes_spawned).expect("Vec<u64> to JSON is infallible");

    conn.execute(
        "INSERT INTO profiles_applied_log \
         (content_hash, profile_name, profile_updated_at_ms, count, \
          panes_spawned_json, recorded_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.content_hash,
            receipt.profile_name,
            receipt.profile_updated_at_ms,
            receipt.count,
            panes_json,
            receipt.recorded_at_ms,
        ],
    )?;
    Ok(())
}

/// Fetch the receipt for `content_hash`. Returns `Ok(None)`
/// when no row matches — the daemon-side handler reads this
/// before issuing real mux spawn calls + short-circuits to the
/// recorded `panes_spawned` when a row exists.
pub fn get_apply_receipt(
    conn: &Connection,
    content_hash: &str,
) -> Result<Option<ApplyReceipt>, ProfilesAppliedLogSqlError> {
    let row = conn
        .query_row(
            "SELECT content_hash, profile_name, profile_updated_at_ms, \
                    count, panes_spawned_json, recorded_at_ms \
             FROM profiles_applied_log \
             WHERE content_hash = ?1",
            params![content_hash],
            |row| {
                let hash: String = row.get(0)?;
                let name: String = row.get(1)?;
                let updated_at: i64 = row.get(2)?;
                let count: u32 = row.get(3)?;
                let panes_json: String = row.get(4)?;
                let recorded_at: i64 = row.get(5)?;
                Ok((hash, name, updated_at, count, panes_json, recorded_at))
            },
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((hash, name, updated_at, count, panes_json, recorded_at)) => {
            let panes_spawned: Vec<u64> = serde_json::from_str(&panes_json).map_err(|e| {
                ProfilesAppliedLogSqlError::Decode {
                    column: "panes_spawned_json",
                    msg: e.to_string(),
                }
            })?;
            Ok(Some(ApplyReceipt {
                content_hash: hash,
                profile_name: name,
                profile_updated_at_ms: updated_at,
                count,
                panes_spawned,
                recorded_at_ms: recorded_at,
            }))
        }
    }
}

/// List receipts for a given profile name in recorded-at-ms
/// descending order. Used by `ft profile history` style
/// operator queries (wired-pass extension).
pub fn list_apply_receipts_for_profile(
    conn: &Connection,
    profile_name: &str,
    limit: u32,
) -> Result<Vec<ApplyReceipt>, ProfilesAppliedLogSqlError> {
    let mut stmt = conn.prepare(
        "SELECT content_hash, profile_name, profile_updated_at_ms, \
                count, panes_spawned_json, recorded_at_ms \
         FROM profiles_applied_log \
         WHERE profile_name = ?1 \
         ORDER BY recorded_at_ms DESC \
         LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![profile_name, limit])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let name: String = row.get(1)?;
        let updated_at: i64 = row.get(2)?;
        let count: u32 = row.get(3)?;
        let panes_json: String = row.get(4)?;
        let recorded_at: i64 = row.get(5)?;
        let panes_spawned: Vec<u64> =
            serde_json::from_str(&panes_json).map_err(|e| ProfilesAppliedLogSqlError::Decode {
                column: "panes_spawned_json",
                msg: e.to_string(),
            })?;
        out.push(ApplyReceipt {
            content_hash: hash,
            profile_name: name,
            profile_updated_at_ms: updated_at,
            count,
            panes_spawned,
            recorded_at_ms: recorded_at,
        });
    }
    Ok(out)
}

/// Delete a receipt by content_hash. Returns the number of rows
/// affected (0 if no match, 1 on success). Operator-tool path
/// for clearing stale receipts; the daemon-side handler does
/// not normally invoke this.
pub fn delete_apply_receipt(
    conn: &Connection,
    content_hash: &str,
) -> Result<usize, ProfilesAppliedLogSqlError> {
    Ok(conn.execute(
        "DELETE FROM profiles_applied_log WHERE content_hash = ?1",
        params![content_hash],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE profiles_applied_log (
                content_hash         TEXT PRIMARY KEY NOT NULL,
                profile_name         TEXT NOT NULL,
                profile_updated_at_ms INTEGER NOT NULL,
                count                INTEGER NOT NULL,
                panes_spawned_json   TEXT NOT NULL DEFAULT '[]',
                recorded_at_ms       INTEGER NOT NULL
            );
            CREATE INDEX profiles_applied_log_profile_name_idx
                ON profiles_applied_log(profile_name);",
        )
        .unwrap();
        conn
    }

    fn sample_receipt() -> ApplyReceipt {
        ApplyReceipt {
            content_hash: "a".repeat(64),
            profile_name: "dev".to_string(),
            profile_updated_at_ms: 1_700_000_000_000,
            count: 3,
            panes_spawned: vec![10, 11, 12],
            recorded_at_ms: 1_700_000_001_000,
        }
    }

    #[test]
    fn insert_then_get_round_trips_receipt() {
        let conn = fresh_db();
        let receipt = sample_receipt();
        insert_apply_receipt(&conn, &receipt).unwrap();
        let fetched = get_apply_receipt(&conn, &receipt.content_hash).unwrap();
        assert_eq!(fetched, Some(receipt));
    }

    #[test]
    fn get_returns_none_for_unknown_hash() {
        let conn = fresh_db();
        let fetched = get_apply_receipt(&conn, &"b".repeat(64)).unwrap();
        assert_eq!(fetched, None);
    }

    #[test]
    fn insert_rejects_bad_content_hash_length() {
        let conn = fresh_db();
        let mut receipt = sample_receipt();
        receipt.content_hash = "deadbeef".to_string(); // not 64 chars
        let err = insert_apply_receipt(&conn, &receipt).unwrap_err();
        match err {
            ProfilesAppliedLogSqlError::Invalid(msg) => assert!(msg.contains("64")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn insert_rejects_non_hex_content_hash() {
        let conn = fresh_db();
        let mut receipt = sample_receipt();
        receipt.content_hash = "Z".repeat(64);
        let err = insert_apply_receipt(&conn, &receipt).unwrap_err();
        assert!(matches!(err, ProfilesAppliedLogSqlError::Invalid(_)));
    }

    #[test]
    fn duplicate_insert_returns_unique_constraint_error() {
        let conn = fresh_db();
        let receipt = sample_receipt();
        insert_apply_receipt(&conn, &receipt).unwrap();
        let err = insert_apply_receipt(&conn, &receipt).unwrap_err();
        match err {
            ProfilesAppliedLogSqlError::Sqlite(rusqlite::Error::SqliteFailure(_, _)) => {}
            other => panic!("expected Sqlite UNIQUE constraint, got {other:?}"),
        }
    }

    #[test]
    fn list_returns_receipts_for_profile_in_recorded_desc() {
        let conn = fresh_db();
        for (i, ts) in [
            (1, 1_700_000_001_000_i64),
            (2, 1_700_000_002_000),
            (3, 1_700_000_003_000),
        ] {
            let receipt = ApplyReceipt {
                content_hash: format!("{}", "a".repeat(63) + &format!("{i}")),
                profile_name: "dev".to_string(),
                profile_updated_at_ms: 1_700_000_000_000,
                count: 1,
                panes_spawned: vec![i as u64 * 10],
                recorded_at_ms: ts,
            };
            insert_apply_receipt(&conn, &receipt).unwrap();
        }
        let fetched = list_apply_receipts_for_profile(&conn, "dev", 10).unwrap();
        assert_eq!(fetched.len(), 3);
        assert!(fetched[0].recorded_at_ms > fetched[1].recorded_at_ms);
        assert!(fetched[1].recorded_at_ms > fetched[2].recorded_at_ms);
    }

    #[test]
    fn list_filters_by_profile_name() {
        let conn = fresh_db();
        let mut r1 = sample_receipt();
        r1.content_hash = "a".repeat(64);
        r1.profile_name = "dev".to_string();
        let mut r2 = sample_receipt();
        r2.content_hash = "b".repeat(64);
        r2.profile_name = "prod".to_string();
        insert_apply_receipt(&conn, &r1).unwrap();
        insert_apply_receipt(&conn, &r2).unwrap();
        let dev_receipts = list_apply_receipts_for_profile(&conn, "dev", 10).unwrap();
        assert_eq!(dev_receipts.len(), 1);
        assert_eq!(dev_receipts[0].content_hash, r1.content_hash);
    }

    #[test]
    fn list_honours_limit() {
        let conn = fresh_db();
        for i in 0..5 {
            let mut r = sample_receipt();
            r.content_hash = format!("{}{}", "a".repeat(63), i);
            r.recorded_at_ms = 1_700_000_000_000 + i as i64;
            insert_apply_receipt(&conn, &r).unwrap();
        }
        let fetched = list_apply_receipts_for_profile(&conn, "dev", 2).unwrap();
        assert_eq!(fetched.len(), 2);
    }

    #[test]
    fn delete_removes_existing_receipt() {
        let conn = fresh_db();
        let receipt = sample_receipt();
        insert_apply_receipt(&conn, &receipt).unwrap();
        let n = delete_apply_receipt(&conn, &receipt.content_hash).unwrap();
        assert_eq!(n, 1);
        let fetched = get_apply_receipt(&conn, &receipt.content_hash).unwrap();
        assert_eq!(fetched, None);
    }

    #[test]
    fn delete_returns_zero_for_unknown() {
        let conn = fresh_db();
        let n = delete_apply_receipt(&conn, &"a".repeat(64)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn get_decodes_panes_spawned_json_back_to_vec_u64() {
        let conn = fresh_db();
        let receipt = ApplyReceipt {
            content_hash: "c".repeat(64),
            profile_name: "dev".to_string(),
            profile_updated_at_ms: 0,
            count: 5,
            panes_spawned: vec![100, 200, 300, 400, 500],
            recorded_at_ms: 1,
        };
        insert_apply_receipt(&conn, &receipt).unwrap();
        let fetched = get_apply_receipt(&conn, &receipt.content_hash)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.panes_spawned, vec![100, 200, 300, 400, 500]);
    }
}
