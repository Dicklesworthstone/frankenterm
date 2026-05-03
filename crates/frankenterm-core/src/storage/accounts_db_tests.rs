//! ft-u6fba Phase 1b: extracted from storage.rs (mod accounts_db_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;
use crate::accounts::AccountRecord;
use rusqlite::{Connection, params};

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    conn
}

fn upsert_account_sync(conn: &Connection, account: &AccountRecord) -> Result<i64> {
    conn.execute(
        "INSERT INTO accounts (
            account_id, service, name, percent_remaining, reset_at,
            tokens_used, tokens_remaining, tokens_limit,
            last_refreshed_at, last_used_at, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(service, account_id) DO UPDATE SET
            name = excluded.name,
            percent_remaining = excluded.percent_remaining,
            reset_at = excluded.reset_at,
            tokens_used = excluded.tokens_used,
            tokens_remaining = excluded.tokens_remaining,
            tokens_limit = excluded.tokens_limit,
            last_refreshed_at = excluded.last_refreshed_at,
            updated_at = excluded.updated_at",
        params![
            account.account_id,
            account.service,
            account.name,
            account.percent_remaining,
            account.reset_at,
            account.tokens_used,
            account.tokens_remaining,
            account.tokens_limit,
            account.last_refreshed_at,
            account.last_used_at,
            account.created_at,
            account.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert account: {e}")))?;

    Ok(conn.last_insert_rowid())
}

fn update_account_last_used_sync(
    conn: &Connection,
    service: &str,
    account_id: &str,
    last_used_at: i64,
) -> Result<()> {
    let updated = conn
        .execute(
            "UPDATE accounts SET last_used_at = ?1, updated_at = ?2
             WHERE service = ?3 AND account_id = ?4",
            params![last_used_at, now_ms(), service, account_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to update account last_used: {e}")))?;

    if updated == 0 {
        return Err(
            StorageError::NotFound(format!("Account not found: {service}/{account_id}")).into(),
        );
    }
    Ok(())
}

fn delete_account_sync(conn: &Connection, service: &str, account_id: &str) -> Result<bool> {
    let deleted = conn
        .execute(
            "DELETE FROM accounts WHERE service = ?1 AND account_id = ?2",
            params![service, account_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to delete account: {e}")))?;

    Ok(deleted > 0)
}

fn get_accounts_by_service_sync(
    conn: &Connection,
    service: &str,
) -> Result<Vec<crate::accounts::AccountRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, service, name, percent_remaining, reset_at,
                    tokens_used, tokens_remaining, tokens_limit,
                    last_refreshed_at, last_used_at, created_at, updated_at
             FROM accounts
             WHERE service = ?1
             ORDER BY percent_remaining DESC, last_used_at ASC NULLS FIRST",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare accounts query: {e}")))?;

    let rows = stmt
        .query_map([service], |row| {
            Ok(crate::accounts::AccountRecord {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                name: row.get(3)?,
                percent_remaining: row.get(4)?,
                reset_at: row.get(5)?,
                tokens_used: row.get(6)?,
                tokens_remaining: row.get(7)?,
                tokens_limit: row.get(8)?,
                last_refreshed_at: row.get(9)?,
                last_used_at: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to query accounts: {e}")))?;

    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(
            row.map_err(|e| StorageError::Database(format!("Failed to read account row: {e}")))?,
        );
    }
    Ok(accounts)
}

fn get_account_sync(
    conn: &Connection,
    service: &str,
    account_id: &str,
) -> Result<Option<crate::accounts::AccountRecord>> {
    let result = conn.query_row(
        "SELECT id, account_id, service, name, percent_remaining, reset_at,
                tokens_used, tokens_remaining, tokens_limit,
                last_refreshed_at, last_used_at, created_at, updated_at
         FROM accounts
         WHERE service = ?1 AND account_id = ?2",
        params![service, account_id],
        |row| {
            Ok(crate::accounts::AccountRecord {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                name: row.get(3)?,
                percent_remaining: row.get(4)?,
                reset_at: row.get(5)?,
                tokens_used: row.get(6)?,
                tokens_remaining: row.get(7)?,
                tokens_limit: row.get(8)?,
                last_refreshed_at: row.get(9)?,
                last_used_at: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        },
    );

    match result {
        Ok(account) => Ok(Some(account)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(format!("Failed to get account: {e}")).into()),
    }
}

fn make_db_account(id: &str, service: &str, pct: f64, now: i64) -> AccountRecord {
    AccountRecord {
        id: 0,
        account_id: id.to_string(),
        service: service.to_string(),
        name: Some(format!("{id}-name")),
        percent_remaining: pct,
        reset_at: None,
        tokens_used: Some(1000),
        tokens_remaining: Some(9000),
        tokens_limit: Some(10000),
        last_refreshed_at: now,
        last_used_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn db_upsert_account_inserts_new() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);

    let row_id = upsert_account_sync(&conn, &acct).unwrap();
    assert!(row_id > 0);

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap();
    assert!(fetched.is_some());
    let f = fetched.unwrap();
    assert_eq!(f.account_id, "acc-1");
    assert_eq!(f.service, "openai");
    assert!((f.percent_remaining - 80.0).abs() < 0.001);
    assert_eq!(f.tokens_used, Some(1000));
}

#[test]
fn db_upsert_account_updates_existing() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);
    upsert_account_sync(&conn, &acct).unwrap();

    // Update with new percent_remaining
    let updated = AccountRecord {
        percent_remaining: 50.0,
        last_refreshed_at: 2000,
        updated_at: 2000,
        tokens_used: Some(5000),
        tokens_remaining: Some(5000),
        ..acct
    };
    upsert_account_sync(&conn, &updated).unwrap();

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
    assert!((fetched.percent_remaining - 50.0).abs() < 0.001);
    assert_eq!(fetched.tokens_used, Some(5000));
    assert_eq!(fetched.last_refreshed_at, 2000);
}

#[test]
fn db_upsert_idempotent() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);

    upsert_account_sync(&conn, &acct).unwrap();
    upsert_account_sync(&conn, &acct).unwrap();
    upsert_account_sync(&conn, &acct).unwrap();

    // Should still be exactly one record
    let accounts = get_accounts_by_service_sync(&conn, "openai").unwrap();
    assert_eq!(accounts.len(), 1);
}

#[test]
fn db_upsert_preserves_last_used_at() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);
    upsert_account_sync(&conn, &acct).unwrap();

    // Set last_used_at via dedicated function
    update_account_last_used_sync(&conn, "openai", "acc-1", 5000).unwrap();

    // Now upsert with new data — should NOT overwrite last_used_at
    // because ON CONFLICT doesn't include last_used_at in the UPDATE
    let updated = AccountRecord {
        percent_remaining: 60.0,
        last_refreshed_at: 2000,
        updated_at: 2000,
        ..make_db_account("acc-1", "openai", 60.0, 2000)
    };
    upsert_account_sync(&conn, &updated).unwrap();

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
    // last_used_at should still be 5000 (set by update_account_last_used_sync)
    assert_eq!(fetched.last_used_at, Some(5000));
    // But percent_remaining should be updated
    assert!((fetched.percent_remaining - 60.0).abs() < 0.001);
}

#[test]
fn db_get_accounts_by_service_sorted() {
    let conn = setup_db();

    // Insert accounts with different percent_remaining
    let accounts = [
        make_db_account("low", "openai", 20.0, 1000),
        make_db_account("high", "openai", 90.0, 1000),
        make_db_account("mid", "openai", 50.0, 1000),
    ];
    for acct in &accounts {
        upsert_account_sync(&conn, acct).unwrap();
    }

    let fetched = get_accounts_by_service_sync(&conn, "openai").unwrap();
    assert_eq!(fetched.len(), 3);
    // Should be sorted by percent_remaining DESC
    assert_eq!(fetched[0].account_id, "high");
    assert_eq!(fetched[1].account_id, "mid");
    assert_eq!(fetched[2].account_id, "low");
}

#[test]
fn db_get_accounts_by_service_nulls_first_for_last_used() {
    let conn = setup_db();

    let a1 = make_db_account("never-used", "openai", 50.0, 1000);
    let a2 = make_db_account("used-recently", "openai", 50.0, 1000);
    upsert_account_sync(&conn, &a1).unwrap();
    upsert_account_sync(&conn, &a2).unwrap();

    // Set last_used_at only for a2
    update_account_last_used_sync(&conn, "openai", "used-recently", 5000).unwrap();

    let fetched = get_accounts_by_service_sync(&conn, "openai").unwrap();
    assert_eq!(fetched.len(), 2);
    // Same percent_remaining, so ordered by last_used_at ASC NULLS FIRST
    assert_eq!(fetched[0].account_id, "never-used");
    assert_eq!(fetched[1].account_id, "used-recently");
}

#[test]
fn db_get_accounts_empty_for_unknown_service() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);
    upsert_account_sync(&conn, &acct).unwrap();

    let fetched = get_accounts_by_service_sync(&conn, "anthropic").unwrap();
    assert!(fetched.is_empty());
}

#[test]
fn db_get_account_returns_none_for_missing() {
    let conn = setup_db();

    let fetched = get_account_sync(&conn, "openai", "nonexistent").unwrap();
    assert!(fetched.is_none());
}

#[test]
fn db_update_last_used_updates_timestamp() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);
    upsert_account_sync(&conn, &acct).unwrap();

    update_account_last_used_sync(&conn, "openai", "acc-1", 9999).unwrap();

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
    assert_eq!(fetched.last_used_at, Some(9999));
}

#[test]
fn db_update_last_used_errors_on_missing() {
    let conn = setup_db();

    let result = update_account_last_used_sync(&conn, "openai", "nonexistent", 1000);
    assert!(result.is_err());
}

#[test]
fn db_delete_account_removes_record() {
    let conn = setup_db();
    let acct = make_db_account("acc-1", "openai", 80.0, 1000);
    upsert_account_sync(&conn, &acct).unwrap();

    let deleted = delete_account_sync(&conn, "openai", "acc-1").unwrap();
    assert!(deleted);

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap();
    assert!(fetched.is_none());
}

#[test]
fn db_delete_nonexistent_returns_false() {
    let conn = setup_db();

    let deleted = delete_account_sync(&conn, "openai", "nonexistent").unwrap();
    assert!(!deleted);
}

#[test]
fn db_multiple_services_isolated() {
    let conn = setup_db();

    upsert_account_sync(&conn, &make_db_account("acc-1", "openai", 80.0, 1000)).unwrap();
    upsert_account_sync(&conn, &make_db_account("acc-2", "anthropic", 60.0, 1000)).unwrap();
    upsert_account_sync(&conn, &make_db_account("acc-3", "openai", 40.0, 1000)).unwrap();

    let openai = get_accounts_by_service_sync(&conn, "openai").unwrap();
    let anthropic = get_accounts_by_service_sync(&conn, "anthropic").unwrap();

    assert_eq!(openai.len(), 2);
    assert_eq!(anthropic.len(), 1);
    assert_eq!(anthropic[0].account_id, "acc-2");
}

#[test]
fn db_upsert_then_select_end_to_end() {
    let conn = setup_db();

    // Insert multiple accounts
    upsert_account_sync(&conn, &make_db_account("depleted", "openai", 2.0, 1000)).unwrap();
    upsert_account_sync(&conn, &make_db_account("best", "openai", 90.0, 1000)).unwrap();
    upsert_account_sync(&conn, &make_db_account("decent", "openai", 50.0, 1000)).unwrap();

    // Fetch and select using the accounts module
    let accounts = get_accounts_by_service_sync(&conn, "openai").unwrap();
    let config = crate::accounts::AccountSelectionConfig::default();
    let result = crate::accounts::select_account(&accounts, &config);

    assert!(result.selected.is_some());
    assert_eq!(result.selected.unwrap().account_id, "best");
    assert_eq!(result.explanation.filtered_out.len(), 1); // "depleted" below 5%
    assert_eq!(result.explanation.candidates.len(), 2); // "best" and "decent"
}

#[test]
fn db_upsert_updates_only_specified_fields() {
    let conn = setup_db();

    // Insert with all fields
    let acct = AccountRecord {
        id: 0,
        account_id: "acc-1".to_string(),
        service: "openai".to_string(),
        name: Some("Original Name".to_string()),
        percent_remaining: 80.0,
        reset_at: Some("2026-02-01T00:00:00Z".to_string()),
        tokens_used: Some(2000),
        tokens_remaining: Some(8000),
        tokens_limit: Some(10000),
        last_refreshed_at: 1000,
        last_used_at: None,
        created_at: 1000,
        updated_at: 1000,
    };
    upsert_account_sync(&conn, &acct).unwrap();

    // Update with changed name and percent
    let updated = AccountRecord {
        name: Some("Updated Name".to_string()),
        percent_remaining: 50.0,
        last_refreshed_at: 2000,
        updated_at: 2000,
        ..acct
    };
    upsert_account_sync(&conn, &updated).unwrap();

    let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
    assert_eq!(fetched.name.as_deref(), Some("Updated Name"));
    assert!((fetched.percent_remaining - 50.0).abs() < 0.001);
    assert_eq!(fetched.reset_at.as_deref(), Some("2026-02-01T00:00:00Z"));
}
