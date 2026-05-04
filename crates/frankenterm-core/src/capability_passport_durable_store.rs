//! Durable SQLite-backed passport store (ft-6j8vl, slice 3 successor
//! of ft-1650n.1).
//!
//! Sits alongside the in-memory [`crate::capability_passport_store::PassportStore`]
//! shipped in slice 2 (which covers the runtime hot path) and the
//! JSONL helpers (which cover incremental import/export). This
//! module ships the persistent surface — a SQLite-backed passport
//! store that survives process restarts at scale.
//!
//! # Schema isolation
//!
//! This substrate does NOT touch [`crate::storage`]'s migration
//! framework. Operators install the durable store as a separate
//! database file (or `:memory:` in tests); the schema is self-
//! initialized on first use via `CREATE TABLE IF NOT EXISTS`.
//! Production deployments that want to merge the schema into the
//! main migrations chain can do so in a follow-up bead — keeping
//! the substrate dependency-free of migrations means it ships
//! without conflicting against sibling agents working on storage.
//!
//! # Generation-monotonicity at the SQL boundary
//!
//! [`DurablePassportStore::upsert`] enforces the same generation-
//! monotonic rule that [`crate::capability_passport_store::PassportValidator`]
//! enforces for the in-memory store: an insert with `generation
//! <= existing.generation` is REJECTED, returning
//! [`UpsertOutcome::RejectedStaleGeneration`]. The check runs in
//! the same transaction as the insert so concurrent writers cannot
//! race past each other.
//!
//! # Eviction
//!
//! [`DurablePassportStore::prune_older_than`] deletes passports
//! whose `signed_at_ms` is older than the supplied threshold —
//! operators tune the threshold to bound the on-disk footprint
//! while preserving recent operator state.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::capability_passport::{CapabilityClass, CapabilityPassport};
use crate::capability_passport_store::PassportKey;

/// Sentinel value stored when a passport is agent-scoped (no
/// pane_id). SQLite NULL on the column would also work, but a
/// sentinel keeps the unique index simple — the composite key is
/// `(agent_id, pane_id)` and `pane_id` IS NULL would require
/// special-casing in WHERE clauses. -1 is unreachable as a
/// `Option<u64>` value, so it's safe as a sentinel.
const AGENT_SCOPED_PANE_SENTINEL: i64 = -1;

/// Errors from [`DurablePassportStore`].
#[derive(Debug)]
pub enum DurablePassportStoreError {
    /// Underlying SQLite error.
    Sql(rusqlite::Error),
    /// Failed to (de)serialize a passport JSON payload.
    Json(serde_json::Error),
    /// Composite key conversion overflowed `i64`.
    KeyOverflow,
}

impl std::fmt::Display for DurablePassportStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "sqlite error: {e}"),
            Self::Json(e) => write!(f, "passport json (de)serialize error: {e}"),
            Self::KeyOverflow => write!(f, "passport key overflowed i64 (pane_id too large)"),
        }
    }
}

impl std::error::Error for DurablePassportStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::KeyOverflow => None,
        }
    }
}

impl From<rusqlite::Error> for DurablePassportStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

impl From<serde_json::Error> for DurablePassportStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// Outcome of an [`DurablePassportStore::upsert`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// Inserted as a new row at this composite key.
    Inserted,
    /// Updated an existing row (incoming generation strictly greater).
    Updated { previous_generation: u64 },
    /// Rejected — incoming generation is `<=` existing generation,
    /// so the upsert would be a stale-or-equal write. Caller should
    /// treat this as "destination already has fresher state".
    RejectedStaleGeneration {
        existing_generation: u64,
        incoming_generation: u64,
    },
}

/// Durable SQLite-backed passport store. Cloning is not supported;
/// callers wrap in `Arc` if shared access is needed.
pub struct DurablePassportStore {
    conn: Mutex<Connection>,
}

impl DurablePassportStore {
    /// Open a passport store at the given filesystem path. Creates
    /// the schema on first use. Use `":memory:"` for tests.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DurablePassportStoreError> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.initialize_schema()?;
        Ok(store)
    }

    /// Open an in-memory passport store (for tests / ephemeral
    /// caches).
    pub fn open_in_memory() -> Result<Self, DurablePassportStoreError> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.initialize_schema()?;
        Ok(store)
    }

    fn initialize_schema(&self) -> Result<(), DurablePassportStoreError> {
        let conn = self.conn.lock().expect("connection mutex poisoned");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS passports (
                 agent_id    TEXT    NOT NULL,
                 pane_id     INTEGER NOT NULL, -- AGENT_SCOPED_PANE_SENTINEL for agent-scoped
                 generation  INTEGER NOT NULL,
                 signed_at_ms INTEGER NOT NULL,
                 payload     TEXT    NOT NULL, -- JSON-serialized CapabilityPassport
                 PRIMARY KEY (agent_id, pane_id)
             );
             CREATE INDEX IF NOT EXISTS idx_passports_agent
                 ON passports(agent_id);
             CREATE INDEX IF NOT EXISTS idx_passports_signed_at
                 ON passports(signed_at_ms);",
        )?;
        Ok(())
    }

    fn pane_id_for_storage(pane_id: Option<u64>) -> Result<i64, DurablePassportStoreError> {
        match pane_id {
            Some(id) => i64::try_from(id).map_err(|_| DurablePassportStoreError::KeyOverflow),
            None => Ok(AGENT_SCOPED_PANE_SENTINEL),
        }
    }

    fn pane_id_from_storage(stored: i64) -> Option<u64> {
        if stored == AGENT_SCOPED_PANE_SENTINEL {
            None
        } else {
            Some(stored as u64)
        }
    }

    /// Insert or update a passport, enforcing generation-monotonic
    /// rule at the SQL transaction boundary.
    pub fn upsert(
        &self,
        passport: &CapabilityPassport,
    ) -> Result<UpsertOutcome, DurablePassportStoreError> {
        let pane_id_storage = Self::pane_id_for_storage(passport.pane_id)?;
        let payload = serde_json::to_string(passport)?;
        let incoming_gen = i64::try_from(passport.generation)
            .map_err(|_| DurablePassportStoreError::KeyOverflow)?;
        let signed_at = i64::try_from(passport.signed_at_ms)
            .map_err(|_| DurablePassportStoreError::KeyOverflow)?;

        let mut conn = self.conn.lock().expect("connection mutex poisoned");
        let tx = conn.transaction()?;

        // Read current state inside the transaction.
        let existing: Option<i64> = tx
            .query_row(
                "SELECT generation FROM passports WHERE agent_id = ?1 AND pane_id = ?2",
                params![&passport.agent_id, pane_id_storage],
                |row| row.get(0),
            )
            .ok();

        let outcome = if let Some(prev_gen_i64) = existing {
            if incoming_gen <= prev_gen_i64 {
                UpsertOutcome::RejectedStaleGeneration {
                    existing_generation: prev_gen_i64 as u64,
                    incoming_generation: passport.generation,
                }
            } else {
                tx.execute(
                    "UPDATE passports SET generation = ?1, signed_at_ms = ?2, payload = ?3
                     WHERE agent_id = ?4 AND pane_id = ?5",
                    params![
                        incoming_gen,
                        signed_at,
                        payload,
                        &passport.agent_id,
                        pane_id_storage,
                    ],
                )?;
                UpsertOutcome::Updated {
                    previous_generation: prev_gen_i64 as u64,
                }
            }
        } else {
            tx.execute(
                "INSERT INTO passports (agent_id, pane_id, generation, signed_at_ms, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &passport.agent_id,
                    pane_id_storage,
                    incoming_gen,
                    signed_at,
                    payload,
                ],
            )?;
            UpsertOutcome::Inserted
        };

        tx.commit()?;
        Ok(outcome)
    }

    /// Fetch a passport by composite key. Returns `None` when the
    /// row is absent.
    pub fn fetch_by_key(
        &self,
        key: &PassportKey,
    ) -> Result<Option<CapabilityPassport>, DurablePassportStoreError> {
        let pane_id_storage = Self::pane_id_for_storage(key.pane_id)?;
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM passports WHERE agent_id = ?1 AND pane_id = ?2",
                params![&key.agent_id, pane_id_storage],
                |row| row.get(0),
            )
            .ok();
        match payload {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// Fetch every passport for `agent_id` across all pane scopes.
    pub fn fetch_by_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<CapabilityPassport>, DurablePassportStoreError> {
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT payload FROM passports WHERE agent_id = ?1 ORDER BY pane_id")?;
        let rows = stmt.query_map(params![agent_id], |row| row.get::<_, String>(0))?;
        let mut passports = Vec::new();
        for row in rows {
            let json = row?;
            passports.push(serde_json::from_str(&json)?);
        }
        Ok(passports)
    }

    /// Fetch every passport that DECLARES (at any verification
    /// level) a capability of the supplied class. Filters in-memory
    /// after pulling all rows — production deployments with large
    /// N can layer a JSON-typed inverted index in a follow-up.
    pub fn fetch_with_class(
        &self,
        class: &CapabilityClass,
    ) -> Result<Vec<CapabilityPassport>, DurablePassportStoreError> {
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let mut stmt = conn.prepare("SELECT payload FROM passports")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut matches = Vec::new();
        for row in rows {
            let json = row?;
            let passport: CapabilityPassport = serde_json::from_str(&json)?;
            if passport.capabilities.iter().any(|e| &e.class == class) {
                matches.push(passport);
            }
        }
        Ok(matches)
    }

    /// Delete every passport with `signed_at_ms < cutoff_ms`.
    /// Returns the number of rows pruned.
    pub fn prune_older_than(&self, cutoff_ms: u64) -> Result<usize, DurablePassportStoreError> {
        let cutoff_i64 =
            i64::try_from(cutoff_ms).map_err(|_| DurablePassportStoreError::KeyOverflow)?;
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let n = conn.execute(
            "DELETE FROM passports WHERE signed_at_ms < ?1",
            params![cutoff_i64],
        )?;
        Ok(n)
    }

    /// Total row count (debug / observability helper).
    pub fn row_count(&self) -> Result<u64, DurablePassportStoreError> {
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM passports", [], |row| row.get(0))?;
        Ok(n as u64)
    }

    /// Remove a passport at the supplied composite key. Returns
    /// true when a row was removed, false when no matching row.
    pub fn delete(&self, key: &PassportKey) -> Result<bool, DurablePassportStoreError> {
        let pane_id_storage = Self::pane_id_for_storage(key.pane_id)?;
        let conn = self.conn.lock().expect("connection mutex poisoned");
        let n = conn.execute(
            "DELETE FROM passports WHERE agent_id = ?1 AND pane_id = ?2",
            params![&key.agent_id, pane_id_storage],
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{CapabilityEntry, CapabilityVerification, RedactedProof};

    fn passport(agent: &str, pane: Option<u64>, generation: u64) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: agent.into(),
            pane_id: pane,
            capabilities: vec![CapabilityEntry {
                class: CapabilityClass::ToolAvailability("bash".into()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(900),
                proof: RedactedProof::from_value(b"proof"),
            }],
            generation,
            signed_at_ms: 1_000 + generation,
        }
    }

    fn store() -> DurablePassportStore {
        DurablePassportStore::open_in_memory().expect("open in-memory store")
    }

    // ── Schema initialization ────────────────────────────────────────────

    #[test]
    fn open_in_memory_initializes_schema() {
        let s = store();
        assert_eq!(s.row_count().unwrap(), 0);
    }

    // ── Insert / fetch roundtrip ─────────────────────────────────────────

    #[test]
    fn insert_and_fetch_pane_scoped_passport_roundtrip() {
        let s = store();
        let p = passport("cc1", Some(7), 1);
        let outcome = s.upsert(&p).unwrap();
        assert_eq!(outcome, UpsertOutcome::Inserted);

        let back = s.fetch_by_key(&PassportKey::pane("cc1", 7)).unwrap();
        assert_eq!(back, Some(p));
        assert_eq!(s.row_count().unwrap(), 1);
    }

    #[test]
    fn insert_and_fetch_agent_scoped_passport_roundtrip() {
        let s = store();
        let p = passport("cc1", None, 1);
        s.upsert(&p).unwrap();
        let back = s.fetch_by_key(&PassportKey::agent("cc1")).unwrap();
        assert_eq!(back, Some(p));
    }

    #[test]
    fn fetch_by_missing_key_returns_none() {
        let s = store();
        assert!(
            s.fetch_by_key(&PassportKey::pane("nobody", 1))
                .unwrap()
                .is_none()
        );
    }

    // ── Generation-monotonic enforcement ─────────────────────────────────

    #[test]
    fn upsert_higher_generation_updates_in_place() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        let outcome = s.upsert(&passport("cc1", Some(1), 2)).unwrap();
        assert_eq!(
            outcome,
            UpsertOutcome::Updated {
                previous_generation: 1
            }
        );
        let back = s
            .fetch_by_key(&PassportKey::pane("cc1", 1))
            .unwrap()
            .unwrap();
        assert_eq!(back.generation, 2);
        assert_eq!(s.row_count().unwrap(), 1);
    }

    #[test]
    fn upsert_equal_generation_is_rejected_as_stale() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 5)).unwrap();
        let outcome = s.upsert(&passport("cc1", Some(1), 5)).unwrap();
        assert_eq!(
            outcome,
            UpsertOutcome::RejectedStaleGeneration {
                existing_generation: 5,
                incoming_generation: 5,
            }
        );
        let back = s
            .fetch_by_key(&PassportKey::pane("cc1", 1))
            .unwrap()
            .unwrap();
        assert_eq!(back.generation, 5);
    }

    #[test]
    fn upsert_lower_generation_is_rejected() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 10)).unwrap();
        let outcome = s.upsert(&passport("cc1", Some(1), 9)).unwrap();
        assert_eq!(
            outcome,
            UpsertOutcome::RejectedStaleGeneration {
                existing_generation: 10,
                incoming_generation: 9,
            }
        );
    }

    // ── Composite key separation ─────────────────────────────────────────

    #[test]
    fn agent_scoped_and_pane_scoped_keys_do_not_collide() {
        let s = store();
        s.upsert(&passport("cc1", None, 1)).unwrap();
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        s.upsert(&passport("cc1", Some(2), 1)).unwrap();
        // 3 distinct rows.
        assert_eq!(s.row_count().unwrap(), 3);
    }

    // ── fetch_by_agent ───────────────────────────────────────────────────

    #[test]
    fn fetch_by_agent_returns_every_pane_scope() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        s.upsert(&passport("cc1", Some(2), 1)).unwrap();
        s.upsert(&passport("cc2", Some(99), 1)).unwrap();
        let cc1 = s.fetch_by_agent("cc1").unwrap();
        assert_eq!(cc1.len(), 2);
        let cc2 = s.fetch_by_agent("cc2").unwrap();
        assert_eq!(cc2.len(), 1);
    }

    // ── fetch_with_class ─────────────────────────────────────────────────

    #[test]
    fn fetch_with_class_filters_by_capability_class() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        s.upsert(&passport("cc2", Some(2), 1)).unwrap();
        // Insert a passport with a DIFFERENT class.
        let mut p_other = passport("cc3", Some(3), 1);
        p_other.capabilities[0].class = CapabilityClass::ToolAvailability("rg".into());
        s.upsert(&p_other).unwrap();

        let bash_holders = s
            .fetch_with_class(&CapabilityClass::ToolAvailability("bash".into()))
            .unwrap();
        assert_eq!(bash_holders.len(), 2);
        let rg_holders = s
            .fetch_with_class(&CapabilityClass::ToolAvailability("rg".into()))
            .unwrap();
        assert_eq!(rg_holders.len(), 1);
        let none_holders = s
            .fetch_with_class(&CapabilityClass::ToolAvailability("nethack".into()))
            .unwrap();
        assert!(none_holders.is_empty());
    }

    // ── Eviction ─────────────────────────────────────────────────────────

    #[test]
    fn prune_older_than_deletes_below_cutoff() {
        let s = store();
        // signed_at_ms = 1_000 + generation; passports with gen=1
        // have signed_at=1001, gen=2 → 1002, etc.
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        s.upsert(&passport("cc2", Some(2), 5)).unwrap();
        s.upsert(&passport("cc3", Some(3), 10)).unwrap();
        // Cutoff between 1005 and 1010 deletes the first two.
        let n = s.prune_older_than(1006).unwrap();
        assert_eq!(n, 2);
        assert_eq!(s.row_count().unwrap(), 1);
        let remaining = s.fetch_by_key(&PassportKey::pane("cc3", 3)).unwrap();
        assert!(remaining.is_some());
    }

    // ── Delete ───────────────────────────────────────────────────────────

    #[test]
    fn delete_returns_true_when_row_present() {
        let s = store();
        s.upsert(&passport("cc1", Some(1), 1)).unwrap();
        assert!(s.delete(&PassportKey::pane("cc1", 1)).unwrap());
        assert!(
            s.fetch_by_key(&PassportKey::pane("cc1", 1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_returns_false_when_row_absent() {
        let s = store();
        assert!(!s.delete(&PassportKey::pane("nobody", 1)).unwrap());
    }

    // ── Concurrency-style smoke ─────────────────────────────────────────

    #[test]
    fn many_upserts_produce_correct_row_count() {
        let s = store();
        for agent_id in 0..50u64 {
            s.upsert(&passport(&format!("agent-{agent_id}"), Some(agent_id), 1))
                .unwrap();
        }
        assert_eq!(s.row_count().unwrap(), 50);
    }
}
