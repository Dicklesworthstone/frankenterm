//! Retroactive redaction backfill for the at-rest output corpus
//! (ft-7h5da.1.3 / W0.3 — "ft redact backfill + derived-store rebuild").
//!
//! Redaction at ingest ([`crate::storage`]'s `redact_segment_for_persistence`)
//! only protects bytes captured *after* a pattern lands in the catalog. Any
//! secret that was persisted before its pattern existed — a "stale-catalog
//! secret" — survives in `output_segments` and in every derived store (FTS5,
//! Tantivy, embeddings) and is returned by search. This module re-applies the
//! *current* catalog to the already-persisted corpus, making redaction
//! **temporal** rather than point-in-time.
//!
//! Guarantees:
//! - **Idempotent.** A re-run rewrites zero rows: once a segment is redacted it
//!   contains no detectable secret, so the next pass's [`Redactor::detect`]
//!   returns empty and the row is left untouched.
//! - **Resumable.** Segments are scanned by ascending `id`; the receipt records
//!   the last id processed so an interrupted pass can continue with
//!   [`RedactBackfillConfig::resume_after_id`].
//! - **Receipted.** Per-pattern-family rewrite counts, the catalog fingerprint,
//!   and the resume cursor are returned and (on a real run) appended to
//!   `maintenance_log` (event_type `redaction_backfill`).
//!
//! This is **offline maintenance** like `ft db repair`: it operates directly on
//! the database file and assumes the writer/watcher is stopped, so it does not
//! contend with the single-writer queue. Rebuilding FTS5/Tantivy is heavy, so
//! callers gate it behind the GC/maintenance budget + operating envelope.
//!
//! Scope note: redaction here is per-segment. A secret split *across* two
//! adjacent segments at capture time would have been caught by the streaming
//! redactor at ingest for new data; for pre-existing data only secrets wholly
//! contained within a single segment are rewritten (the dominant stale-catalog
//! case). Cross-segment-split backfill is tracked separately.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::redactor::{Redactor, secret_pattern_names};
use crate::{Error, Result};

/// Schema tag for the backfill receipt envelope.
pub const REDACT_BACKFILL_SCHEMA_VERSION: &str = "ft.redact-backfill.v1";

/// Default number of segments read per scan batch.
pub const DEFAULT_BACKFILL_BATCH_SIZE: u32 = 1_000;

/// `maintenance_log.event_type` used for backfill receipts.
pub const MAINTENANCE_EVENT_TYPE: &str = "redaction_backfill";

/// Maximum `segment_id` values bound into a single `DELETE ... IN (...)`.
const EMBEDDING_DELETE_CHUNK: usize = 500;

/// Configuration for a single redaction-backfill pass.
#[derive(Debug, Clone)]
pub struct RedactBackfillConfig {
    /// Segments read per batch. Clamped to at least 1.
    pub batch_size: u32,
    /// Scan and count, but never mutate the database.
    pub dry_run: bool,
    /// Resume strictly after this segment id (exclusive). `0` starts at the beginning.
    pub resume_after_id: i64,
    /// Rebuild the `output_segments_fts` FTS5 index after rewriting (ignored on dry-run).
    pub rebuild_fts: bool,
    /// Delete `segment_embeddings` rows for rewritten segments so stale
    /// (pre-redaction) vectors stop being served and are regenerated lazily.
    pub invalidate_embeddings: bool,
}

impl Default for RedactBackfillConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BACKFILL_BATCH_SIZE,
            dry_run: false,
            resume_after_id: 0,
            rebuild_fts: true,
            invalidate_embeddings: true,
        }
    }
}

/// Outcome of a backfill pass. Serialized into the `maintenance_log` metadata
/// and returned to the CLI for human/JSON rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactBackfillReceipt {
    /// Receipt schema tag ([`REDACT_BACKFILL_SCHEMA_VERSION`]).
    pub schema_version: String,
    /// Whether this was a dry run (no mutations).
    pub dry_run: bool,
    /// Fingerprint of the active pattern catalog (sha256 over ordered family names).
    pub catalog_version: String,
    /// Number of pattern families evaluated.
    pub patterns_checked: usize,
    /// Total segments scanned.
    pub segments_scanned: u64,
    /// Segments that contained at least one secret (rewritten unless `dry_run`).
    pub segments_rewritten: u64,
    /// Per-family count of secret occurrences detected/redacted.
    pub family_counts: BTreeMap<String, u64>,
    /// Highest segment id processed — the resume cursor.
    pub last_segment_id: i64,
    /// `segment_embeddings` rows invalidated for rewritten segments.
    pub embeddings_invalidated: u64,
    /// Whether the FTS5 index was rebuilt.
    pub fts_rebuilt: bool,
    /// Tantivy lexical index rebuild is signaled (performed out-of-process by the
    /// search daemon); recorded for honesty rather than performed inline.
    pub tantivy_rebuild_requested: bool,
}

impl RedactBackfillReceipt {
    fn new(dry_run: bool, resume_after_id: i64) -> Self {
        Self {
            schema_version: REDACT_BACKFILL_SCHEMA_VERSION.to_string(),
            dry_run,
            catalog_version: catalog_version(),
            patterns_checked: secret_pattern_names().count(),
            segments_scanned: 0,
            segments_rewritten: 0,
            family_counts: BTreeMap::new(),
            last_segment_id: resume_after_id,
            embeddings_invalidated: 0,
            fts_rebuilt: false,
            tantivy_rebuild_requested: false,
        }
    }

    /// One-line human summary for the `maintenance_log` message column / CLI.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mode = if self.dry_run { "dry-run" } else { "applied" };
        format!(
            "redact backfill [{mode}]: scanned={} rewritten={} families={} embeddings_invalidated={} fts_rebuilt={} cursor={}",
            self.segments_scanned,
            self.segments_rewritten,
            self.family_counts.len(),
            self.embeddings_invalidated,
            self.fts_rebuilt,
            self.last_segment_id,
        )
    }
}

/// Fingerprint of the active secret-pattern catalog: sha256 over the ordered
/// family names. Mirrors `backup.rs`'s catalog-version scheme so a backfill
/// receipt and a backup manifest produced from the same catalog agree.
#[must_use]
pub fn catalog_version() -> String {
    let names: Vec<&'static str> = secret_pattern_names().collect();
    let mut hasher = Sha256::new();
    hasher.update(names.join("\n").as_bytes());
    format!(
        "live-secret-patterns-sha256:{}",
        hex::encode(hasher.finalize())
    )
}

/// The active catalog fingerprint, computed once and cached. Hot paths — most
/// importantly segment-append stamping (ft-7h5da.1.5) — call this instead of
/// recomputing the sha256 over the pattern names on every row.
#[must_use]
pub fn current_catalog_version() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(catalog_version)
}

fn db_err(context: &str, err: &rusqlite::Error) -> Error {
    Error::Storage(crate::StorageError::Database(format!("{context}: {err}")))
}

/// Re-apply the current redaction catalog to at-rest `output_segments` rows and
/// rebuild the derived lexical stores.
///
/// The caller must hold exclusive access to `conn` (writer/watcher stopped),
/// matching the offline-maintenance contract of `ft db repair`.
pub fn run_redact_backfill(
    conn: &Connection,
    config: &RedactBackfillConfig,
) -> Result<RedactBackfillReceipt> {
    let redactor = Redactor::new();
    let batch = i64::from(config.batch_size.max(1));
    let mut cursor = config.resume_after_id;
    let mut receipt = RedactBackfillReceipt::new(config.dry_run, config.resume_after_id);
    let mut rewritten_ids: Vec<i64> = Vec::new();

    // secure_delete keeps overwritten secret bytes out of freed pages. Harmless
    // (and skipped from any effect) under dry-run since we never write.
    if !config.dry_run {
        conn.execute_batch("PRAGMA secure_delete=ON")
            .map_err(|e| db_err("enabling secure_delete for backfill", &e))?;
    }

    loop {
        let rows = read_segment_batch(conn, cursor, batch)?;
        if rows.is_empty() {
            break;
        }
        for (id, content) in &rows {
            receipt.segments_scanned += 1;
            cursor = *id;
            receipt.last_segment_id = *id;

            let detections = redactor.detect(content);
            if detections.is_empty() {
                continue;
            }
            for (family, _start, _end) in &detections {
                *receipt
                    .family_counts
                    .entry((*family).to_string())
                    .or_insert(0) += 1;
            }
            receipt.segments_rewritten += 1;

            if !config.dry_run {
                let redacted = redactor.redact(content);
                let content_len = i64::try_from(redacted.len()).map_err(|_| {
                    Error::Storage(crate::StorageError::Database(format!(
                        "redacted output_segments content length exceeds i64 for id {id}"
                    )))
                })?;
                conn.execute(
                    "UPDATE output_segments
                     SET content = ?1, content_len = ?2, content_hash = NULL
                     WHERE id = ?3",
                    rusqlite::params![redacted, content_len, id],
                )
                .map_err(|e| db_err(&format!("rewriting output_segments id {id}"), &e))?;
                rewritten_ids.push(*id);
            }
        }
    }

    if !config.dry_run && !rewritten_ids.is_empty() {
        if config.invalidate_embeddings && table_exists(conn, "segment_embeddings")? {
            receipt.embeddings_invalidated = invalidate_embeddings(conn, &rewritten_ids)?;
        }
        if config.rebuild_fts && table_exists(conn, "output_segments_fts")? {
            conn.execute_batch(
                "INSERT INTO output_segments_fts(output_segments_fts) VALUES('rebuild')",
            )
            .map_err(|e| db_err("rebuilding output_segments_fts", &e))?;
            receipt.fts_rebuilt = true;
        }
        // The Tantivy index lives on disk and is owned by the search daemon; it
        // re-derives from output_segments out-of-process. We signal rather than
        // reindex inline so the heavy rebuild stays under the daemon's budget.
        receipt.tantivy_rebuild_requested = true;

        persist_receipt(conn, &receipt)?;
    }

    Ok(receipt)
}

/// Convenience wrapper that opens `db_path` directly and runs a backfill pass.
///
/// Intended for the `ft redact backfill` CLI: it keeps the binary crate free of
/// a direct `rusqlite` dependency. This opens its own connection, so the
/// watcher/writer should be stopped first (offline-maintenance contract).
pub fn run_redact_backfill_on_path(
    db_path: &str,
    config: &RedactBackfillConfig,
) -> Result<RedactBackfillReceipt> {
    let conn = Connection::open(db_path)
        .map_err(|e| db_err(&format!("opening database {db_path}"), &e))?;
    run_redact_backfill(&conn, config)
}

// ── Targeted purge (ft-7h5da.1.4) ──────────────────────────────────────────

/// Schema tag for the targeted-purge receipt envelope.
pub const REDACT_PURGE_SCHEMA_VERSION: &str = "ft.redact-purge.v1";

/// `maintenance_log.event_type` used for purge receipts (append-only tombstone).
pub const PURGE_MAINTENANCE_EVENT_TYPE: &str = "redaction_purge";

/// Length of a SHA-256 hex digest (the `--secret-hash` key).
const SECRET_HASH_HEX_LEN: usize = 64;

/// Whether `h` is a syntactically valid SHA-256 hex digest (64 hex chars).
#[must_use]
pub fn is_valid_secret_hash(h: &str) -> bool {
    h.len() == SECRET_HASH_HEX_LEN && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Outcome of a targeted secret purge (`ft redact purge --secret-hash`).
///
/// The target secret is identified **only** by its SHA-256 hash — the plaintext
/// never appears in this receipt, in the `maintenance_log` row, or in the CLI
/// argv (ft-7h5da.1.4 leak hygiene). Serialized into the `maintenance_log`
/// metadata as an append-only tombstone and returned to the CLI for rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactPurgeReceipt {
    /// Receipt schema tag ([`REDACT_PURGE_SCHEMA_VERSION`]).
    pub schema_version: String,
    /// Whether this was a dry run (no mutations).
    pub dry_run: bool,
    /// SHA-256 hex of the targeted secret — **never** the plaintext.
    pub secret_hash: String,
    /// Total segments scanned.
    pub segments_scanned: u64,
    /// Segments that contained at least one occurrence of the target secret
    /// (rewritten unless `dry_run`).
    pub segments_purged: u64,
    /// Total occurrences of the target secret excised across all segments.
    pub occurrences_purged: u64,
    /// Per-pattern-family occurrence counts of the target secret.
    pub family_counts: BTreeMap<String, u64>,
    /// Highest segment id processed — the resume cursor.
    pub last_segment_id: i64,
    /// `segment_embeddings` rows invalidated for rewritten segments.
    pub embeddings_invalidated: u64,
    /// Whether the FTS5 index was rebuilt.
    pub fts_rebuilt: bool,
    /// Tantivy lexical index rebuild is signaled (performed out-of-process).
    pub tantivy_rebuild_requested: bool,
    /// True when occurrences were found: backup archives predating this purge may
    /// still contain the secret and must be reviewed / re-created. The offline DB
    /// purge cannot rewrite sealed backup files, so it flags rather than excises
    /// them (the CLI enumerates the actual archives).
    pub backups_review_required: bool,
}

impl RedactPurgeReceipt {
    fn new(dry_run: bool, secret_hash: &str, resume_after_id: i64) -> Self {
        Self {
            schema_version: REDACT_PURGE_SCHEMA_VERSION.to_string(),
            dry_run,
            secret_hash: secret_hash.to_string(),
            segments_scanned: 0,
            segments_purged: 0,
            occurrences_purged: 0,
            family_counts: BTreeMap::new(),
            last_segment_id: resume_after_id,
            embeddings_invalidated: 0,
            fts_rebuilt: false,
            tantivy_rebuild_requested: false,
            backups_review_required: false,
        }
    }

    /// One-line human summary for the `maintenance_log` message column / CLI.
    /// Carries the hash, never the plaintext.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mode = if self.dry_run { "dry-run" } else { "applied" };
        format!(
            "redact purge [{mode}] hash={}: scanned={} purged_segments={} occurrences={} embeddings_invalidated={} fts_rebuilt={} backups_review={} cursor={}",
            self.secret_hash,
            self.segments_scanned,
            self.segments_purged,
            self.occurrences_purged,
            self.embeddings_invalidated,
            self.fts_rebuilt,
            self.backups_review_required,
            self.last_segment_id,
        )
    }
}

/// Excise every occurrence of one specific secret — identified solely by its
/// SHA-256 hash — from at-rest `output_segments` and the derived lexical stores,
/// emitting an append-only tombstone receipt (ft-7h5da.1.4).
///
/// The secret is located by hashing each catalog-detected match in a segment and
/// comparing to `secret_hash`; the plaintext is never required as an argument and
/// never persisted. Segments that contain the target are re-redacted with the
/// full catalog (safe over-redaction — every catalog secret in an affected
/// segment should be at-rest-redacted anyway). Only secrets the active catalog
/// can detect are purgeable, which matches the secret-scan reporting surface that
/// produced the hash in the first place.
///
/// The caller must hold exclusive access to `conn` (writer/watcher stopped),
/// matching the offline-maintenance contract.
///
/// # Errors
/// Returns an error if `secret_hash` is not a 64-char hex digest, or on any
/// database failure during scan / rewrite / rebuild / receipt persistence.
pub fn run_redact_purge(
    conn: &Connection,
    secret_hash: &str,
    config: &RedactBackfillConfig,
) -> Result<RedactPurgeReceipt> {
    // Fail closed on a malformed key so we never scan the corpus with a garbage
    // hash (which would silently match nothing and read as "clean").
    let secret_hash = secret_hash.trim().to_ascii_lowercase();
    if !is_valid_secret_hash(&secret_hash) {
        return Err(Error::Storage(crate::StorageError::Database(format!(
            "redact purge: --secret-hash must be a 64-char SHA-256 hex digest (got {} chars)",
            secret_hash.len()
        ))));
    }

    let redactor = Redactor::new();
    let batch = i64::from(config.batch_size.max(1));
    let mut cursor = config.resume_after_id;
    let mut receipt = RedactPurgeReceipt::new(config.dry_run, &secret_hash, config.resume_after_id);
    let mut rewritten_ids: Vec<i64> = Vec::new();

    if !config.dry_run {
        conn.execute_batch("PRAGMA secure_delete=ON")
            .map_err(|e| db_err("enabling secure_delete for purge", &e))?;
    }

    loop {
        let rows = read_segment_batch(conn, cursor, batch)?;
        if rows.is_empty() {
            break;
        }
        for (id, content) in &rows {
            receipt.segments_scanned += 1;
            cursor = *id;
            receipt.last_segment_id = *id;

            let mut segment_occurrences: u64 = 0;
            for (family, start, end) in redactor.detect(content) {
                // `start`/`end` are byte offsets from the same redactor's regex
                // match on `content` (always valid char-boundary ranges); `get`
                // keeps the hash-compare panic-free regardless.
                let Some(matched) = content.get(start..end) else {
                    continue;
                };
                if crate::secrets::hash_secret(matched) == secret_hash {
                    segment_occurrences += 1;
                    *receipt.family_counts.entry(family.to_string()).or_insert(0) += 1;
                }
            }
            if segment_occurrences == 0 {
                continue;
            }
            receipt.segments_purged += 1;
            receipt.occurrences_purged += segment_occurrences;

            if !config.dry_run {
                let redacted = redactor.redact(content);
                let content_len = i64::try_from(redacted.len()).map_err(|_| {
                    Error::Storage(crate::StorageError::Database(format!(
                        "redacted output_segments content length exceeds i64 for id {id}"
                    )))
                })?;
                conn.execute(
                    "UPDATE output_segments
                     SET content = ?1, content_len = ?2, content_hash = NULL
                     WHERE id = ?3",
                    rusqlite::params![redacted, content_len, id],
                )
                .map_err(|e| db_err(&format!("purging output_segments id {id}"), &e))?;
                rewritten_ids.push(*id);
            }
        }
    }

    // Any segment that held the secret means sealed backups predating this purge
    // may still carry it — flag for operator review regardless of dry-run.
    receipt.backups_review_required = receipt.segments_purged > 0;

    if !config.dry_run && !rewritten_ids.is_empty() {
        if config.invalidate_embeddings && table_exists(conn, "segment_embeddings")? {
            receipt.embeddings_invalidated = invalidate_embeddings(conn, &rewritten_ids)?;
        }
        if config.rebuild_fts && table_exists(conn, "output_segments_fts")? {
            conn.execute_batch(
                "INSERT INTO output_segments_fts(output_segments_fts) VALUES('rebuild')",
            )
            .map_err(|e| db_err("rebuilding output_segments_fts", &e))?;
            receipt.fts_rebuilt = true;
        }
        receipt.tantivy_rebuild_requested = true;
        persist_purge_receipt(conn, &receipt)?;
    }

    Ok(receipt)
}

/// Convenience wrapper that opens `db_path` directly and runs a targeted purge.
/// Intended for the `ft redact purge` CLI (keeps the binary crate free of a
/// direct `rusqlite` dependency). Opens its own connection, so the
/// watcher/writer must be stopped first (offline-maintenance contract).
///
/// # Errors
/// Propagates [`run_redact_purge`] errors plus connection-open failures.
pub fn run_redact_purge_on_path(
    db_path: &str,
    secret_hash: &str,
    config: &RedactBackfillConfig,
) -> Result<RedactPurgeReceipt> {
    let conn = Connection::open(db_path)
        .map_err(|e| db_err(&format!("opening database {db_path}"), &e))?;
    run_redact_purge(&conn, secret_hash, config)
}

fn persist_purge_receipt(conn: &Connection, receipt: &RedactPurgeReceipt) -> Result<()> {
    if !table_exists(conn, "maintenance_log")? {
        return Ok(());
    }
    let metadata = serde_json::to_string(receipt).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "serializing redact-purge receipt: {e}"
        )))
    })?;
    conn.execute(
        "INSERT INTO maintenance_log (event_type, message, metadata, timestamp)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            PURGE_MAINTENANCE_EVENT_TYPE,
            receipt.summary_line(),
            metadata,
            crate::storage::now_ms(),
        ],
    )
    .map_err(|e| db_err("recording redact-purge maintenance receipt", &e))?;
    Ok(())
}

fn read_segment_batch(conn: &Connection, after_id: i64, limit: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn
        .prepare("SELECT id, content FROM output_segments WHERE id > ?1 ORDER BY id LIMIT ?2")
        .map_err(|e| db_err("preparing output_segments scan", &e))?;
    let mapped = stmt
        .query_map(rusqlite::params![after_id, limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| db_err("scanning output_segments", &e))?;
    let mut out = Vec::new();
    for row in mapped {
        out.push(row.map_err(|e| db_err("reading output_segments row", &e))?);
    }
    Ok(out)
}

fn invalidate_embeddings(conn: &Connection, segment_ids: &[i64]) -> Result<u64> {
    let mut deleted: u64 = 0;
    for chunk in segment_ids.chunks(EMBEDDING_DELETE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM segment_embeddings WHERE segment_id IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let n = conn
            .execute(&sql, params.as_slice())
            .map_err(|e| db_err("invalidating segment_embeddings", &e))?;
        deleted += n as u64;
    }
    Ok(deleted)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .map_err(|e| db_err(&format!("checking for table {name}"), &e))?;
    Ok(count > 0)
}

fn persist_receipt(conn: &Connection, receipt: &RedactBackfillReceipt) -> Result<()> {
    if !table_exists(conn, "maintenance_log")? {
        return Ok(());
    }
    let metadata = serde_json::to_string(receipt).map_err(|e| {
        Error::Storage(crate::StorageError::Database(format!(
            "serializing redact-backfill receipt: {e}"
        )))
    })?;
    conn.execute(
        "INSERT INTO maintenance_log (event_type, message, metadata, timestamp)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            MAINTENANCE_EVENT_TYPE,
            receipt.summary_line(),
            metadata,
            crate::storage::now_ms(),
        ],
    )
    .map_err(|e| db_err("recording redact-backfill maintenance receipt", &e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal subset of the real schema sufficient to exercise the backfill:
    /// `output_segments` + its FTS5 mirror + `segment_embeddings` + `maintenance_log`.
    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                content_hash TEXT,
                captured_at INTEGER NOT NULL,
                UNIQUE(pane_id, seq)
            );
            CREATE VIRTUAL TABLE output_segments_fts USING fts5(
                content, content='output_segments', content_rowid='id'
            );
            CREATE TABLE segment_embeddings (
                segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                embedded_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (segment_id, embedder_id)
            );
            CREATE TABLE maintenance_log (
                id INTEGER PRIMARY KEY,
                event_type TEXT NOT NULL,
                message TEXT,
                metadata TEXT,
                timestamp INTEGER NOT NULL
            );",
        )
        .expect("create schema");
        conn
    }

    fn insert_segment(conn: &Connection, id: i64, pane: i64, seq: i64, content: &str) {
        conn.execute(
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, content_hash, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'orig-hash', 0)",
            rusqlite::params![id, pane, seq, content, content.len() as i64],
        )
        .expect("insert segment");
        conn.execute(
            "INSERT INTO output_segments_fts(rowid, content) VALUES (?1, ?2)",
            rusqlite::params![id, content],
        )
        .expect("insert fts");
        conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector)
             VALUES (?1, 'test-embedder', 1, x'00')",
            rusqlite::params![id],
        )
        .expect("insert embedding");
    }

    /// A string the live catalog must redact, used to seed "stale-catalog" rows.
    /// Guarded so a catalog change that drops coverage fails loudly here.
    const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

    #[test]
    fn secret_fixture_is_detected_by_live_catalog() {
        let r = Redactor::new();
        assert!(
            !r.detect(SECRET).is_empty(),
            "test fixture secret must be covered by the live catalog"
        );
        assert!(!r.redact(SECRET).contains(SECRET));
    }

    #[test]
    fn backfill_redacts_at_rest_secret_and_clears_hash() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("export AWS_KEY={SECRET}"));
        insert_segment(&conn, 2, 0, 1, "clean line, no secrets here");

        let receipt = run_redact_backfill(&conn, &RedactBackfillConfig::default()).unwrap();

        assert_eq!(receipt.segments_scanned, 2);
        assert_eq!(receipt.segments_rewritten, 1);
        assert!(!receipt.family_counts.is_empty());
        assert_eq!(receipt.last_segment_id, 2);
        assert!(receipt.fts_rebuilt);
        assert!(receipt.tantivy_rebuild_requested);

        // The secret is gone from the at-rest row, the cached length matches,
        // and the overlap hash was invalidated.
        let (content, len, hash): (String, i64, Option<String>) = conn
            .query_row(
                "SELECT content, content_len, content_hash FROM output_segments WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(!content.contains(SECRET));
        assert_eq!(len, content.len() as i64);
        assert!(hash.is_none());

        // FTS no longer returns the secret.
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'AKIAIOSFODNN7EXAMPLE'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 0, "FTS index still returns the redacted secret");

        // Embeddings for the rewritten segment were invalidated; the clean one kept.
        assert_eq!(receipt.embeddings_invalidated, 1);
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM segment_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 1);

        // Receipt was persisted.
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![MAINTENANCE_EVENT_TYPE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[test]
    fn backfill_is_idempotent() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("token {SECRET} end"));

        let first = run_redact_backfill(&conn, &RedactBackfillConfig::default()).unwrap();
        assert_eq!(first.segments_rewritten, 1);

        let second = run_redact_backfill(&conn, &RedactBackfillConfig::default()).unwrap();
        assert_eq!(second.segments_rewritten, 0, "re-run must be a no-op");
        assert_eq!(second.embeddings_invalidated, 0);
        assert!(!second.fts_rebuilt, "no rewrites => no FTS rebuild");

        // Only the first pass wrote a receipt.
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![MAINTENANCE_EVENT_TYPE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[test]
    fn dry_run_counts_without_mutating() {
        let conn = setup_db();
        let original = format!("key={SECRET}");
        insert_segment(&conn, 1, 0, 0, &original);

        let cfg = RedactBackfillConfig {
            dry_run: true,
            ..RedactBackfillConfig::default()
        };
        let receipt = run_redact_backfill(&conn, &cfg).unwrap();

        assert!(receipt.dry_run);
        assert_eq!(receipt.segments_rewritten, 1);
        assert!(!receipt.family_counts.is_empty());
        assert!(!receipt.fts_rebuilt);
        assert_eq!(receipt.embeddings_invalidated, 0);

        // Nothing changed on disk.
        let content: String = conn
            .query_row(
                "SELECT content FROM output_segments WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content, original);
        let logged: i64 = conn
            .query_row("SELECT count(*) FROM maintenance_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 0, "dry-run must not write a receipt");
    }

    #[test]
    fn resume_after_id_skips_earlier_segments() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("first {SECRET}"));
        insert_segment(&conn, 2, 0, 1, &format!("second {SECRET}"));

        let cfg = RedactBackfillConfig {
            resume_after_id: 1,
            ..RedactBackfillConfig::default()
        };
        let receipt = run_redact_backfill(&conn, &cfg).unwrap();

        // Only segment 2 is in scope.
        assert_eq!(receipt.segments_scanned, 1);
        assert_eq!(receipt.segments_rewritten, 1);
        assert_eq!(receipt.last_segment_id, 2);

        // Segment 1 was skipped: still contains the secret.
        let one: String = conn
            .query_row(
                "SELECT content FROM output_segments WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(one.contains(SECRET));
    }

    #[test]
    fn small_batch_size_processes_all_segments() {
        let conn = setup_db();
        for i in 1..=5 {
            insert_segment(&conn, i, 0, i, &format!("line {i} {SECRET}"));
        }
        let cfg = RedactBackfillConfig {
            batch_size: 2,
            ..RedactBackfillConfig::default()
        };
        let receipt = run_redact_backfill(&conn, &cfg).unwrap();
        assert_eq!(receipt.segments_scanned, 5);
        assert_eq!(receipt.segments_rewritten, 5);
        assert_eq!(receipt.last_segment_id, 5);
    }

    #[test]
    fn catalog_version_is_stable_and_namespaced() {
        let v = catalog_version();
        assert_eq!(v, catalog_version());
        assert!(v.starts_with("live-secret-patterns-sha256:"));
    }

    // ── Targeted purge (ft-7h5da.1.4) ──────────────────────────────────────

    #[test]
    fn purge_excises_secret_by_hash_and_emits_receipt() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("export AWS_KEY={SECRET}"));
        insert_segment(&conn, 2, 0, 1, "clean line, no secrets here");
        let hash = crate::secrets::hash_secret(SECRET);

        let receipt = run_redact_purge(&conn, &hash, &RedactBackfillConfig::default()).unwrap();

        // Acceptance: all occurrences removed; receipt names the surfaces touched.
        assert_eq!(receipt.secret_hash, hash);
        assert_eq!(receipt.segments_scanned, 2);
        assert_eq!(receipt.segments_purged, 1);
        assert_eq!(receipt.occurrences_purged, 1);
        assert!(!receipt.family_counts.is_empty());
        assert!(receipt.fts_rebuilt);
        assert!(receipt.tantivy_rebuild_requested);
        assert!(receipt.backups_review_required);

        // Secret excised from the at-rest row + overlap hash invalidated.
        let (content, hash_col): (String, Option<String>) = conn
            .query_row(
                "SELECT content, content_hash FROM output_segments WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(!content.contains(SECRET));
        assert!(hash_col.is_none());

        // FTS no longer returns the secret.
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'AKIAIOSFODNN7EXAMPLE'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 0);

        // Embeddings invalidated for the purged segment only.
        assert_eq!(receipt.embeddings_invalidated, 1);

        // Append-only tombstone persisted under the purge event type.
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![PURGE_MAINTENANCE_EVENT_TYPE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1);
    }

    #[test]
    fn purge_receipt_and_log_never_contain_plaintext() {
        // Leak hygiene: the secret plaintext must never appear in the receipt
        // JSON, the maintenance_log message, or its metadata — only the hash.
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("key={SECRET}"));
        let hash = crate::secrets::hash_secret(SECRET);
        let receipt = run_redact_purge(&conn, &hash, &RedactBackfillConfig::default()).unwrap();

        let receipt_json = serde_json::to_string(&receipt).unwrap();
        assert!(!receipt_json.contains(SECRET), "receipt leaked plaintext");
        assert!(receipt_json.contains(&hash));

        let (msg, meta): (String, String) = conn
            .query_row(
                "SELECT message, metadata FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![PURGE_MAINTENANCE_EVENT_TYPE],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            !msg.contains(SECRET),
            "maintenance_log message leaked plaintext"
        );
        assert!(
            !meta.contains(SECRET),
            "maintenance_log metadata leaked plaintext"
        );
        assert!(meta.contains(&hash));
    }

    #[test]
    fn purge_nonmatching_hash_is_noop() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("key={SECRET}"));
        // A valid-format hash that matches no secret in the corpus.
        let other = crate::secrets::hash_secret("AKIAXXXXXXXXXXXXXXXX");
        let receipt = run_redact_purge(&conn, &other, &RedactBackfillConfig::default()).unwrap();
        assert_eq!(receipt.segments_purged, 0);
        assert_eq!(receipt.occurrences_purged, 0);
        assert!(!receipt.backups_review_required);
        assert!(!receipt.fts_rebuilt);
        // The real secret is untouched (this purge targeted a different hash).
        let content: String = conn
            .query_row(
                "SELECT content FROM output_segments WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(content.contains(SECRET));
        // No tombstone for a no-op.
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![PURGE_MAINTENANCE_EVENT_TYPE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(logged, 0);
    }

    #[test]
    fn purge_dry_run_does_not_mutate() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("key={SECRET}"));
        let hash = crate::secrets::hash_secret(SECRET);
        let cfg = RedactBackfillConfig {
            dry_run: true,
            ..RedactBackfillConfig::default()
        };
        let receipt = run_redact_purge(&conn, &hash, &cfg).unwrap();
        assert!(receipt.dry_run);
        assert_eq!(receipt.segments_purged, 1);
        assert_eq!(receipt.occurrences_purged, 1);
        assert!(!receipt.fts_rebuilt, "dry-run must not rebuild");
        // At-rest content unchanged.
        let content: String = conn
            .query_row(
                "SELECT content FROM output_segments WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(content.contains(SECRET), "dry-run must not mutate content");
        // No tombstone on dry-run.
        let logged: i64 = conn
            .query_row(
                "SELECT count(*) FROM maintenance_log WHERE event_type = ?1",
                rusqlite::params![PURGE_MAINTENANCE_EVENT_TYPE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(logged, 0);
    }

    #[test]
    fn purge_rejects_malformed_hash_and_normalizes_case() {
        let conn = setup_db();
        insert_segment(&conn, 1, 0, 0, &format!("key={SECRET}"));
        assert!(run_redact_purge(&conn, "not-a-hash", &RedactBackfillConfig::default()).is_err());
        assert!(run_redact_purge(&conn, "abc", &RedactBackfillConfig::default()).is_err());
        // Uppercase hex is normalized and accepted (still matches the secret).
        let up = crate::secrets::hash_secret(SECRET).to_uppercase();
        let receipt = run_redact_purge(&conn, &up, &RedactBackfillConfig::default()).unwrap();
        assert_eq!(receipt.segments_purged, 1);
    }

    #[test]
    fn is_valid_secret_hash_checks_len_and_charset() {
        assert!(is_valid_secret_hash(&crate::secrets::hash_secret(SECRET)));
        assert!(!is_valid_secret_hash(""));
        assert!(!is_valid_secret_hash("zz"));
        assert!(!is_valid_secret_hash(&"a".repeat(63)));
        assert!(!is_valid_secret_hash(&"g".repeat(64)));
    }
}
