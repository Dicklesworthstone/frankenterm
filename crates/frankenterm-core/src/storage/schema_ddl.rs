//! Schema Definition (DDL strings & version constant)
//!
//! [ft-6qkx1 / ft-dn2tu Phase 2] Extracted from `storage.rs` so the
//! initial DDL surface lives in a focused, append-only file instead of
//! a 30k-line mega-module. Re-exported via `pub use schema_ddl::{...}`
//! in `storage.rs` so existing call sites in
//! `frankenterm_core::storage::SCHEMA_VERSION` / `SCHEMA_SQL` and the
//! sibling modules need no edits.
//!
//! Constants exported:
//! - [`SCHEMA_VERSION`] — current target schema version (PRAGMA user_version).
//! - [`SCHEMA_SQL`] — full DDL bundle applied for fresh DB initialization.
//! - [`FTS_TRIGGER_RECREATE_SQL`] — `pub(crate)` idempotent FTS-trigger
//!   re-creation, used by `defer_fts_triggers: false` open paths in
//!   `storage.rs` and test scaffolding.
//!
//! Pure data: no impls, no helpers — moves are mechanical.

// =============================================================================
// Schema Definition
// =============================================================================

/// Current schema version for migration tracking.
///
/// This is the target version that new databases will be initialized to,
/// and existing databases will be migrated to.
/// Uses SQLite's PRAGMA user_version for atomic version tracking.
///
/// Per ft-4yr9i: bumped 24 → 25 to gate the agent_profiles table
/// and role index from agent_profiles.rs (ft-df3cz substrate). The
/// migration entry sits at MIGRATIONS[24] in storage/migrations.rs.
///
/// Per br-ft-4iz0q substrate-pass: bumped 25 → 26 to gate the
/// `profiles_applied_log` table the daemon-side
/// `RobotProfile.apply` handler writes idempotency receipts
/// into. The migration entry sits at MIGRATIONS[25] in
/// storage/migrations.rs; the receipt schema mirrors the
/// `ApplyReceipt` substrate type at
/// crates/frankenterm-core/src/robot_profile_handler.rs.
///
/// Per ft-27rlg: bumped 26 → 27 to gate the
/// `fleet_mutation_receipts` table used by non-dry-run
/// `ft robot fleet scale` / `rebalance` durable receipt replay.
///
/// Per ft-7h5da.8.1: bumped 27 → 28 to gate the durable
/// `limit_windows` ledger for pane/account rate-limit forecasting.
///
/// Per ft-7h5da.1.5: bumped 28 → 29 to stamp `output_segments` with the
/// redaction catalog version in effect at capture (corpus-hygiene queries).
/// Per ft-ayy9x: bumped 29 → 30 to normalize `segment_embeddings.embedded_at`
/// from epoch seconds to epoch milliseconds (schema-wide ms convention).
/// Per ft-7h5da.2.3: bumped 30 → 31 to stamp best-effort semantic zone type
/// metadata on new output segments for zone-scoped historical search.
/// Per ft-wi24o: bumped 31 → 32 to repair the `segment_embeddings.embedded_at`
/// column DEFAULT (still epoch seconds on DBs upgraded through v22/v23) to the
/// schema-wide epoch-ms convention; v30 only fixed existing row values.
/// Bumped 32 → 33 to add expiring, token-owned event-delivery leases. These
/// leases let stream consumers reserve an unhandled event, flush it downstream,
/// and only then atomically finalize `handled_at` without a crash-loss window.
/// Bumped 33 → 34 to make event-retention holes authoritative: a durable
/// cursor epoch and high-water mark identify the history whose deletions can be
/// proven, while canonical inclusive intervals record every committed deletion
/// inside the current epoch's authoritative evidence range.
/// Bumped 34 → 35 to replace the constant-key unhandled-event index with
/// order- and identity-specific partial indexes and add a per-pane
/// newest-output index. These indexes bound the storage work behind large
/// event streams and pane-activity snapshots without changing persisted
/// records.
/// Bumped 35 → 36 to separate restorable snapshots from restore bookkeeping
/// receipts and bind each snapshot to its own topology instead of the mutable
/// session-level latest topology. Deterministic per-session, global, and
/// snapshot-only latest indexes bound newest/list queries without deep sorts.
/// Bumped 36 → 37 to bind `shutdown_clean = 1` to the exact checkpoint row
/// whose durable receipt justified that claim. Deleting or pruning that row can
/// now invalidate clean state deterministically instead of leaving a stale
/// clean flag behind.
/// Bumped 37 → 38 to make checkpoint identities non-reusable, introduce an
/// explicit restore-intent role, and reserve a unique intent-to-outcome link.
/// Causal authority now follows the never-reused checkpoint ID; wall-clock
/// timestamps remain presentation and retention-age data.
/// Bumped 38 → 39 to make retained scrollback metadata an O(panes) snapshot
/// projection. `pane_scrollback_summary` is maintained transactionally by the
/// same SQLite statements that append or prune `output_segments`; snapshots no
/// longer aggregate an unbounded pane history on every capture.
/// Bumped 39 → 40 to make the session-retention byte budget exact and
/// O(sessions) at decision time. `session_retained_size` charges every stored
/// session/checkpoint/pane/lifecycle field exactly once under a documented
/// logical-byte contract and is maintained by the owning row mutations.
/// Bumped 40 -> 41 to fence unclean session ownership to a host boot and
/// process incarnation, persist capture heartbeats, and record explicit
/// recovery-retention acknowledgement without conflating live sessions with
/// stale crash candidates. Bumped 41 -> 42 so databases created by the first
/// v41 implementation migrate to the same all-or-none owner-tuple enforcement
/// as fresh databases instead of silently retaining weaker trigger authority.
/// Bumped 42 -> 43 so pane children beneath legacy orphan checkpoints remain
/// collectible without weakening retained-size authority for live sessions.
/// Bumped 43 -> 44 to replace unbounded recovery-candidate validation during
/// retention with trigger-invalidated, incrementally reconciled usability and
/// durable bounded-selection authority.
pub const SCHEMA_VERSION: i32 = 44;

/// [ft-ih4tm] Idempotent re-creation of the three `output_segments` FTS
/// triggers. Called when a database is opened with
/// `defer_fts_triggers: false` so the flag is truly reversible — without
/// this, a DB opened once with `true` stays in deferred mode forever
/// because `initialize_schema` short-circuits for up-to-date schemas and
/// `CREATE TRIGGER IF NOT EXISTS` from the main `SCHEMA_SQL` never re-runs.
///
/// KEEP IN SYNC with the `CREATE TRIGGER IF NOT EXISTS output_segments_*`
/// block inside `SCHEMA_SQL` below — if a trigger body changes here,
/// change it there too and vice versa.
pub(crate) const FTS_TRIGGER_RECREATE_SQL: &str = r"
CREATE TRIGGER IF NOT EXISTS output_segments_ai AFTER INSERT ON output_segments BEGIN
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_ad AFTER DELETE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_au AFTER UPDATE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;
";

/// Schema initialization SQL.
///
/// Convention notes:
///
/// - Timestamps: epoch milliseconds (i64) for hot-path queries.
/// - JSON columns: TEXT containing JSON (v0 simplicity).
/// - All tables use INTEGER PRIMARY KEY for rowid aliasing.
pub const SCHEMA_SQL: &str = r#"
-- Enable WAL mode for concurrent reads and single-writer semantics
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

-- Schema version tracking
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at INTEGER NOT NULL,  -- epoch ms
    description TEXT
);

-- ft metadata: version compatibility + provenance
CREATE TABLE IF NOT EXISTS ft_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    min_compatible_ft TEXT NOT NULL,
    created_by_ft TEXT NOT NULL,
    created_at INTEGER NOT NULL  -- epoch ms
);

-- Panes: metadata and observation decisions
-- Supports: ft status, ft robot state, privacy/perf filtering
CREATE TABLE IF NOT EXISTS panes (
    pane_id INTEGER PRIMARY KEY,
    pane_uuid TEXT,                    -- stable UUID (persists across renames/moves)
    domain TEXT NOT NULL DEFAULT 'local',
    window_id INTEGER,
    tab_id INTEGER,
    title TEXT,
    cwd TEXT,
    tty_name TEXT,
    first_seen_at INTEGER NOT NULL,   -- epoch ms
    last_seen_at INTEGER NOT NULL,    -- epoch ms
    observed INTEGER NOT NULL DEFAULT 1,  -- bool: 1=observe, 0=ignore
    ignore_reason TEXT,               -- rule id or short description if ignored
    last_decision_at INTEGER          -- epoch ms when observed/ignore was set
);

CREATE INDEX IF NOT EXISTS idx_panes_last_seen ON panes(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_panes_observed ON panes(observed);

-- Output segments: append-only terminal output capture
-- UNIQUE(pane_id, seq) enforces monotonic sequence per pane
CREATE TABLE IF NOT EXISTS output_segments (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,             -- monotonically increasing within pane
    content TEXT NOT NULL,
    content_len INTEGER NOT NULL,     -- cached length for stats
    content_hash TEXT,                -- for overlap detection (optional)
    captured_at INTEGER NOT NULL,     -- epoch ms
    redaction_catalog_version TEXT,   -- redaction catalog fingerprint at capture (ft-7h5da.1.5); NULL = unknown
    zone_type TEXT,                   -- best-effort semantic zone at capture (prompt/input/output); NULL = untyped/unavailable
    UNIQUE(pane_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_segments_pane_seq ON output_segments(pane_id, seq);
CREATE INDEX IF NOT EXISTS idx_segments_captured ON output_segments(captured_at);
CREATE INDEX IF NOT EXISTS idx_segments_pane_captured
    ON output_segments(pane_id, captured_at DESC);
CREATE INDEX IF NOT EXISTS idx_segments_zone_type ON output_segments(zone_type);

-- Exact retained-segment metadata for bounded snapshot projection (ft-0yuxe.4).
--
-- This is deliberately one row per persisted pane, including panes with no
-- retained output. A segment is an arbitrary captured stream fragment; it is
-- not a logical line, a CR-delimited record, or a wrapped display row.
CREATE TABLE IF NOT EXISTS pane_scrollback_summary (
    pane_id INTEGER PRIMARY KEY
        REFERENCES panes(pane_id) ON DELETE NO ACTION
        DEFERRABLE INITIALLY DEFERRED,
    retained_segment_count INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(retained_segment_count) = 'integer' AND retained_segment_count >= 0),
    first_seq INTEGER
        CHECK(first_seq IS NULL OR (typeof(first_seq) = 'integer' AND first_seq >= 0)),
    last_seq INTEGER
        CHECK(last_seq IS NULL OR (typeof(last_seq) = 'integer' AND last_seq >= 0)),
    first_captured_at INTEGER
        CHECK(first_captured_at IS NULL OR
              (typeof(first_captured_at) = 'integer' AND first_captured_at >= 0)),
    last_captured_at INTEGER
        CHECK(last_captured_at IS NULL OR
              (typeof(last_captured_at) = 'integer' AND last_captured_at >= 0)),
    CHECK(
        (retained_segment_count = 0 AND
         first_seq IS NULL AND last_seq IS NULL AND
         first_captured_at IS NULL AND last_captured_at IS NULL)
        OR
        (retained_segment_count > 0 AND
         first_seq IS NOT NULL AND last_seq IS NOT NULL AND first_seq <= last_seq AND
         first_captured_at IS NOT NULL AND last_captured_at IS NOT NULL AND
         first_captured_at <= last_captured_at)
    )
);

CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_panes_ai
AFTER INSERT ON panes BEGIN
    INSERT INTO pane_scrollback_summary (pane_id) VALUES (new.pane_id);
END;

CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_panes_ad
AFTER DELETE ON panes BEGIN
    DELETE FROM pane_scrollback_summary WHERE pane_id = old.pane_id;
END;

CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_bi
BEFORE INSERT ON pane_scrollback_summary
WHEN NOT EXISTS (SELECT 1 FROM panes WHERE pane_id = new.pane_id) BEGIN
    SELECT RAISE(ABORT, 'scrollback summary requires a persisted pane');
END;

CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_bd
BEFORE DELETE ON pane_scrollback_summary
WHEN EXISTS (SELECT 1 FROM panes WHERE pane_id = old.pane_id) BEGIN
    SELECT RAISE(ABORT, 'live pane scrollback summary is permanent');
END;

CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_pane_id_bu
BEFORE UPDATE OF pane_id ON pane_scrollback_summary
WHEN new.pane_id != old.pane_id BEGIN
    SELECT RAISE(ABORT, 'scrollback summary pane identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_bi
BEFORE INSERT ON output_segments
WHEN typeof(new.seq) != 'integer' OR new.seq < 0 OR
     typeof(new.captured_at) != 'integer' OR new.captured_at < 0 OR
     NOT EXISTS (
         SELECT 1 FROM pane_scrollback_summary WHERE pane_id = new.pane_id
     ) BEGIN
    SELECT RAISE(ABORT, 'invalid output segment metadata or missing scrollback summary');
END;

CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_ai
AFTER INSERT ON output_segments BEGIN
    UPDATE pane_scrollback_summary
    SET retained_segment_count = retained_segment_count + 1,
        first_seq = CASE
            WHEN retained_segment_count = 0 THEN new.seq
            ELSE min(first_seq, new.seq)
        END,
        last_seq = CASE
            WHEN retained_segment_count = 0 THEN new.seq
            ELSE max(last_seq, new.seq)
        END,
        first_captured_at = CASE
            WHEN retained_segment_count = 0 THEN new.captured_at
            ELSE min(first_captured_at, new.captured_at)
        END,
        last_captured_at = CASE
            WHEN retained_segment_count = 0 THEN new.captured_at
            ELSE max(last_captured_at, new.captured_at)
        END
    WHERE pane_id = new.pane_id;
END;

CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_bd
BEFORE DELETE ON output_segments
WHEN EXISTS (SELECT 1 FROM panes WHERE pane_id = old.pane_id) AND
     NOT EXISTS (
         SELECT 1 FROM pane_scrollback_summary WHERE pane_id = old.pane_id
     ) BEGIN
    SELECT RAISE(ABORT, 'missing scrollback summary during output retention');
END;

CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_ad
AFTER DELETE ON output_segments BEGIN
    UPDATE pane_scrollback_summary
    SET retained_segment_count = retained_segment_count - 1,
        first_seq = CASE
            WHEN retained_segment_count = 1 THEN NULL
            WHEN old.seq = first_seq THEN (
                SELECT min(seq) FROM output_segments WHERE pane_id = old.pane_id
            )
            ELSE first_seq
        END,
        last_seq = CASE
            WHEN retained_segment_count = 1 THEN NULL
            WHEN old.seq = last_seq THEN (
                SELECT max(seq) FROM output_segments WHERE pane_id = old.pane_id
            )
            ELSE last_seq
        END,
        first_captured_at = CASE
            WHEN retained_segment_count = 1 THEN NULL
            WHEN old.captured_at = first_captured_at THEN (
                SELECT min(captured_at) FROM output_segments WHERE pane_id = old.pane_id
            )
            ELSE first_captured_at
        END,
        last_captured_at = CASE
            WHEN retained_segment_count = 1 THEN NULL
            WHEN old.captured_at = last_captured_at THEN (
                SELECT max(captured_at) FROM output_segments WHERE pane_id = old.pane_id
            )
            ELSE last_captured_at
        END
    WHERE pane_id = old.pane_id;
END;

CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_metadata_bu
BEFORE UPDATE OF pane_id, seq, captured_at ON output_segments
WHEN new.pane_id != old.pane_id OR new.seq != old.seq OR
     new.captured_at != old.captured_at BEGIN
    SELECT RAISE(ABORT, 'output segment scrollback metadata is immutable');
END;

-- Segment embeddings for semantic search
CREATE TABLE IF NOT EXISTS segment_embeddings (
    segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
    embedder_id TEXT NOT NULL,
    dimension INTEGER NOT NULL,
    vector BLOB NOT NULL,
    -- epoch MILLISECONDS (schema-wide convention); ft-ayy9x. strftime('%s')
    -- is epoch seconds, so scale by 1000. Writers also provide ms explicitly.
    embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
    PRIMARY KEY (segment_id, embedder_id)
);

CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder
    ON segment_embeddings(embedder_id);

-- Output gaps: explicit discontinuities in capture
CREATE TABLE IF NOT EXISTS output_gaps (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq_before INTEGER NOT NULL,      -- last known seq before gap
    seq_after INTEGER NOT NULL,       -- first seq after gap
    reason TEXT NOT NULL,             -- e.g., "daemon_restart", "timeout", "buffer_overflow"
    detected_at INTEGER NOT NULL      -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_gaps_pane ON output_gaps(pane_id);
CREATE INDEX IF NOT EXISTS idx_gaps_detected ON output_gaps(detected_at);

-- FTS5 virtual table for full-text search over segments
CREATE VIRTUAL TABLE IF NOT EXISTS output_segments_fts USING fts5(
    content,
    content='output_segments',
    content_rowid='id',
    tokenize='porter unicode61'
);

-- Triggers to keep FTS index in sync
CREATE TRIGGER IF NOT EXISTS output_segments_ai AFTER INSERT ON output_segments BEGIN
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_ad AFTER DELETE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_au AFTER UPDATE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Events: pattern detections with lifecycle tracking
-- Supports: unhandled queries, workflow linkage, idempotency
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    rule_id TEXT NOT NULL,            -- stable pattern identifier
    agent_type TEXT NOT NULL,         -- codex, claude_code, gemini, unknown
    event_type TEXT NOT NULL,         -- detection category
    severity TEXT NOT NULL,           -- info, warning, critical
    confidence REAL NOT NULL,         -- 0.0-1.0
    extracted TEXT,                   -- JSON: structured data from pattern
    matched_text TEXT,                -- original matched text
    segment_id INTEGER REFERENCES output_segments(id),  -- source segment
    detected_at INTEGER NOT NULL,     -- epoch ms

    -- Lifecycle tracking
    handled_at INTEGER,               -- epoch ms when handled (NULL = unhandled)
    handled_by_workflow_id TEXT,      -- links to workflow_executions.id
    handled_status TEXT,              -- completed, aborted, failed, paused

    -- Triage state tracking (bd-1yk8)
    triage_state TEXT,                -- e.g. new, investigating, resolved
    triage_updated_at INTEGER,        -- epoch ms
    triage_updated_by TEXT,           -- actor identifier (optional)

    -- Idempotency: optional dedupe key (pane_id + rule_id + time_window)
    dedupe_key TEXT,                  -- computed key for duplicate prevention

    -- Two-phase downstream-delivery lease. A live token reserves an otherwise
    -- unhandled event; handled_at stays NULL until the token owner confirms its
    -- downstream write + flush. Expiry makes the token stealable for crash
    -- recovery; it does not itself invalidate ownership before a steal.
    -- Keep these append-only v33 columns at the table tail so fresh and
    -- v32-upgraded databases have identical PRAGMA table_info order.
    delivery_lease_token TEXT,
    delivery_lease_acquired_at INTEGER,
    delivery_lease_expires_at INTEGER,

    UNIQUE(dedupe_key)                -- prevents duplicate events when dedupe_key set
);

CREATE INDEX IF NOT EXISTS idx_events_pane ON events(pane_id);
CREATE INDEX IF NOT EXISTS idx_events_rule ON events(rule_id);
CREATE INDEX IF NOT EXISTS idx_events_unhandled_detected
    ON events(detected_at DESC, id DESC) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_unhandled_id
    ON events(id ASC) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_unhandled_pane
    ON events(pane_id ASC) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_detected ON events(detected_at);
CREATE INDEX IF NOT EXISTS idx_events_severity ON events(severity, detected_at);
CREATE INDEX IF NOT EXISTS idx_events_triage_state
    ON events(triage_state) WHERE triage_state IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_events_segment_id
    ON events(segment_id) WHERE segment_id IS NOT NULL;

-- Durable, exact evidence for event-retention holes.  A missing row in
-- `events` is not by itself proof of retention: cursor streams can skip IDs
-- because of pane, handled-state, or other filters.  Cleanup records only IDs
-- it actually deletes in the interval table, in the same transaction as the
-- DELETE.  Intervals are disjoint and maximally coalesced by the writer.
--
-- `evidence_from_event_id` is 1 on a fresh database.  A v33 upgrade initializes
-- it to one past the greatest then-live event ID and marks legacy history
-- incomplete because deletions performed by older binaries cannot be
-- reconstructed honestly.  `cursor_epoch` prevents IDs reused across that
-- upgrade boundary from aliasing an old durable cursor.  The writer also
-- rotates the epoch and advances the evidence boundary if the exact interval
-- ledger reaches its bounded row ceiling.
CREATE TABLE IF NOT EXISTS event_retention_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    cursor_epoch TEXT NOT NULL CHECK (
        length(cursor_epoch) = 32
        AND cursor_epoch NOT GLOB '*[^0-9a-f]*'
    ),
    legacy_history_complete INTEGER NOT NULL CHECK (legacy_history_complete IN (0, 1)),
    generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
    evidence_from_event_id INTEGER NOT NULL CHECK (evidence_from_event_id > 0),
    max_event_id INTEGER NOT NULL DEFAULT 0
        CHECK (max_event_id >= 0 AND max_event_id >= evidence_from_event_id - 1),
    deleted_event_count INTEGER NOT NULL DEFAULT 0 CHECK (deleted_event_count >= 0),
    last_deleted_at INTEGER CHECK (last_deleted_at IS NULL OR last_deleted_at >= 0),
    CHECK (legacy_history_complete = 0 OR evidence_from_event_id = 1),
    CHECK (deleted_event_count >= generation),
    CHECK (
        (generation = 0 AND deleted_event_count = 0 AND last_deleted_at IS NULL)
        OR (generation > 0 AND deleted_event_count > 0 AND last_deleted_at IS NOT NULL)
    )
);

INSERT OR IGNORE INTO event_retention_state (
    singleton, cursor_epoch, legacy_history_complete,
    generation, evidence_from_event_id, max_event_id,
    deleted_event_count, last_deleted_at
) VALUES (1, lower(hex(randomblob(16))), 1, 0, 1, 0, 0, NULL);

CREATE TABLE IF NOT EXISTS event_retention_intervals (
    start_id INTEGER PRIMARY KEY CHECK (start_id > 0),
    end_id INTEGER NOT NULL CHECK (end_id >= start_id),
    first_generation INTEGER NOT NULL CHECK (first_generation > 0),
    last_generation INTEGER NOT NULL CHECK (last_generation >= first_generation),
    first_deleted_at INTEGER NOT NULL CHECK (first_deleted_at >= 0),
    last_deleted_at INTEGER NOT NULL CHECK (last_deleted_at >= first_deleted_at)
);

-- Supports the hot resume lookup: first deleted interval whose end is after
-- the caller's cursor.  The insert/update guards below enforce disjoint,
-- maximally coalesced ordering.
CREATE UNIQUE INDEX IF NOT EXISTS idx_event_retention_intervals_end
    ON event_retention_intervals(end_id);

-- Empty outside a retention transaction.  The DELETE guard below makes pane
-- cascades and ad-hoc SQL fail closed instead of creating an unrecorded cursor
-- hole.  The writer fills these exact IDs, records their intervals, and clears
-- the authorizations before committing the same transaction.
CREATE TABLE IF NOT EXISTS event_retention_delete_authorizations (
    event_id INTEGER PRIMARY KEY CHECK (event_id > 0)
);

-- Also empty outside the bounded-ledger rotation transaction. Without this
-- guard, an unrelated `DELETE FROM event_retention_intervals` could erase
-- authoritative hole evidence and turn a real loss into false completeness.
CREATE TABLE IF NOT EXISTS event_retention_rotation_authorizations (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1)
);

CREATE TRIGGER IF NOT EXISTS event_retention_state_delete_guard
BEFORE DELETE ON event_retention_state
BEGIN
    SELECT RAISE(ABORT, 'event retention state is permanent');
END;

-- These values are trust boundaries, not mutable counters that may be
-- rewound.  In particular, lowering the evidence boundary or changing legacy
-- history from incomplete to complete could turn an unknowable resume range
-- into a false no-pruning answer; lowering the high-water mark could permit ID
-- reuse.  Legitimate writers only advance these fields (epoch rotation changes
-- complete -> incomplete), so fail closed on every reverse transition.
CREATE TRIGGER IF NOT EXISTS event_retention_state_monotonic_guard
BEFORE UPDATE ON event_retention_state
WHEN NEW.generation < OLD.generation
  OR NEW.evidence_from_event_id < OLD.evidence_from_event_id
  OR NEW.max_event_id < OLD.max_event_id
  OR NEW.deleted_event_count < OLD.deleted_event_count
  OR (
      OLD.last_deleted_at IS NOT NULL
      AND (NEW.last_deleted_at IS NULL OR NEW.last_deleted_at < OLD.last_deleted_at)
  )
  OR (OLD.legacy_history_complete = 0 AND NEW.legacy_history_complete = 1)
BEGIN
    SELECT RAISE(ABORT, 'event retention authority cannot move backwards');
END;

-- Epoch, evidence-boundary, and legacy-completeness changes are one atomic
-- authority transition.  They may occur only during the exact rotation that
-- also clears the old epoch's interval rows.  This prevents standalone SQL
-- from minting a cursor token whose evidence does not match its epoch.
CREATE TRIGGER IF NOT EXISTS event_retention_state_rotation_guard
BEFORE UPDATE ON event_retention_state
WHEN (
        NEW.cursor_epoch != OLD.cursor_epoch
        OR NEW.evidence_from_event_id != OLD.evidence_from_event_id
        OR NEW.legacy_history_complete != OLD.legacy_history_complete
     )
 AND (
        NOT EXISTS (
            SELECT 1 FROM event_retention_rotation_authorizations
            WHERE singleton = 1
        )
        OR NEW.cursor_epoch = OLD.cursor_epoch
        OR NEW.legacy_history_complete != 0
        OR OLD.max_event_id >= 9223372036854775807
        OR NEW.evidence_from_event_id != OLD.max_event_id + 1
        OR NEW.generation != OLD.generation
        OR NEW.max_event_id != OLD.max_event_id
        OR NEW.deleted_event_count != OLD.deleted_event_count
        OR NEW.last_deleted_at IS NOT OLD.last_deleted_at
     )
BEGIN
    SELECT RAISE(ABORT, 'event retention epoch rotation must be atomic and authorized');
END;

-- Make interval invalidation a schema invariant rather than relying solely on
-- one Rust call site.  The authorization row is still present while this
-- AFTER trigger runs, so the interval-delete guard accepts this exact clear;
-- statement/transaction rollback restores both state and intervals on error.
CREATE TRIGGER IF NOT EXISTS event_retention_state_rotation_clear_intervals
AFTER UPDATE OF cursor_epoch ON event_retention_state
WHEN NEW.cursor_epoch != OLD.cursor_epoch
BEGIN
    DELETE FROM event_retention_intervals;
END;

CREATE TRIGGER IF NOT EXISTS event_retention_intervals_insert_guard
BEFORE INSERT ON event_retention_intervals
WHEN EXISTS (
    SELECT 1 FROM event_retention_intervals AS existing
    WHERE existing.end_id >= CASE
              WHEN NEW.start_id > 1 THEN NEW.start_id - 1 ELSE 1
          END
      AND existing.start_id <= CASE
              WHEN NEW.end_id < 9223372036854775807 THEN NEW.end_id + 1
              ELSE 9223372036854775807
          END
)
BEGIN
    SELECT RAISE(ABORT, 'event retention intervals must be disjoint and non-adjacent');
END;

CREATE TRIGGER IF NOT EXISTS event_retention_intervals_update_guard
BEFORE UPDATE ON event_retention_intervals
BEGIN
    SELECT RAISE(ABORT, 'event retention intervals are replace-only');
END;

CREATE TRIGGER IF NOT EXISTS event_retention_intervals_delete_guard
BEFORE DELETE ON event_retention_intervals
WHEN NOT EXISTS (SELECT 1 FROM event_retention_delete_authorizations)
 AND NOT EXISTS (
     SELECT 1 FROM event_retention_rotation_authorizations WHERE singleton = 1
 )
BEGIN
    SELECT RAISE(ABORT, 'event retention interval deletion requires an authorized batch or epoch rotation');
END;

-- SQLite may reuse rowids after the largest event is deleted (and starts again
-- at 1 after a full prune).  Reuse would make a durable deletion interval lie
-- about the new row, so reject any insert that does not advance the durable
-- high-water mark.  The production writer allocates max_event_id + 1.
CREATE TRIGGER IF NOT EXISTS events_monotonic_id_guard
BEFORE INSERT ON events
WHEN NOT EXISTS (
         SELECT 1 FROM event_retention_state WHERE singleton = 1
     )
     OR NEW.id <= COALESCE((
         SELECT max_event_id FROM event_retention_state WHERE singleton = 1
     ), 9223372036854775807)
BEGIN
    SELECT RAISE(ABORT, 'events.id must advance the durable event high-water mark');
END;

CREATE TRIGGER IF NOT EXISTS events_monotonic_id_advance
AFTER INSERT ON events
BEGIN
    UPDATE event_retention_state
    SET max_event_id = NEW.id
    WHERE singleton = 1;
END;

CREATE TRIGGER IF NOT EXISTS events_id_update_guard
BEFORE UPDATE OF id ON events
WHEN NEW.id != OLD.id
BEGIN
    SELECT RAISE(ABORT, 'events.id is immutable once allocated');
END;

CREATE TRIGGER IF NOT EXISTS events_retention_delete_guard
BEFORE DELETE ON events
WHEN NOT EXISTS (
    SELECT 1 FROM event_retention_delete_authorizations
    WHERE event_id = OLD.id
)
BEGIN
    SELECT RAISE(ABORT, 'event deletion requires transactional retention evidence');
END;

-- Event labels (many-to-one) for triage and filtering (bd-1yk8)
CREATE TABLE IF NOT EXISTS event_labels (
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL,      -- epoch ms
    created_by TEXT,                 -- actor identifier (optional)
    PRIMARY KEY (event_id, label)
);

CREATE INDEX IF NOT EXISTS idx_event_labels_event ON event_labels(event_id);
CREATE INDEX IF NOT EXISTS idx_event_labels_label ON event_labels(label);

-- Event notes (one-to-one) for operator annotations (bd-1yk8)
CREATE TABLE IF NOT EXISTS event_notes (
    event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    note TEXT NOT NULL,
    updated_at INTEGER NOT NULL,      -- epoch ms
    updated_by TEXT                  -- actor identifier (optional)
);

CREATE INDEX IF NOT EXISTS idx_event_notes_updated_at ON event_notes(updated_at);

-- Event mutes: suppress noisy notifications by identity key
CREATE TABLE IF NOT EXISTS event_mutes (
    identity_key TEXT PRIMARY KEY,
    scope TEXT NOT NULL DEFAULT 'workspace',
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    created_by TEXT,
    reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_event_mutes_expires
    ON event_mutes(expires_at) WHERE expires_at IS NOT NULL;

-- Agent sessions: per-agent session timeline with token tracking
CREATE TABLE IF NOT EXISTS agent_sessions (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    agent_type TEXT NOT NULL,         -- codex, claude_code, gemini, unknown
    session_id TEXT,                  -- Agent's internal session ID if available
    external_id TEXT,                 -- Correlation with cass, etc.
    external_meta TEXT,               -- JSON metadata for correlation decisions
    started_at INTEGER NOT NULL,      -- epoch ms
    ended_at INTEGER,                 -- epoch ms (NULL = still active)
    end_reason TEXT,                  -- completed, limit_reached, error, manual
    -- Token tracking
    total_tokens INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cached_tokens INTEGER,
    reasoning_tokens INTEGER,
    -- Model info
    model_name TEXT,
    -- Cost tracking
    estimated_cost_usd REAL
);

CREATE INDEX IF NOT EXISTS idx_sessions_pane ON agent_sessions(pane_id, started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_external ON agent_sessions(external_id) WHERE external_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_active ON agent_sessions(ended_at) WHERE ended_at IS NULL;

-- Workflow executions: durable FSM state for resumability
CREATE TABLE IF NOT EXISTS workflow_executions (
    id TEXT PRIMARY KEY,              -- UUID or ulid
    workflow_name TEXT NOT NULL,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    trigger_event_id INTEGER REFERENCES events(id),  -- event that started this
    current_step INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running',  -- running, waiting, completed, aborted
    wait_condition TEXT,              -- JSON: WaitCondition if status='waiting'
    context TEXT,                     -- JSON: workflow-specific state
    result TEXT,                      -- JSON: final result if completed
    error TEXT,                       -- error message if aborted
    started_at INTEGER NOT NULL,      -- epoch ms
    updated_at INTEGER NOT NULL,      -- epoch ms
    completed_at INTEGER              -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_workflows_pane ON workflow_executions(pane_id);
CREATE INDEX IF NOT EXISTS idx_workflows_status ON workflow_executions(status);
CREATE INDEX IF NOT EXISTS idx_workflows_started ON workflow_executions(started_at);
CREATE INDEX IF NOT EXISTS idx_workflows_trigger_event_id
    ON workflow_executions(trigger_event_id) WHERE trigger_event_id IS NOT NULL;

-- Workflow step logs: execution history for audit and debugging
CREATE TABLE IF NOT EXISTS workflow_step_logs (
    id INTEGER PRIMARY KEY,
    workflow_id TEXT NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE,
    audit_action_id INTEGER REFERENCES audit_actions(id) ON DELETE SET NULL,
    step_index INTEGER NOT NULL,
    step_name TEXT NOT NULL,
    step_id TEXT,
    step_kind TEXT,
    result_type TEXT NOT NULL,        -- continue, done, retry, abort, wait_for
    result_data TEXT,                 -- JSON: result payload
    policy_summary TEXT,              -- JSON: decision summary
    verification_refs TEXT,           -- JSON: verification evidence refs
    error_code TEXT,                  -- stable error code if step failed
    started_at INTEGER NOT NULL,      -- epoch ms
    completed_at INTEGER NOT NULL,    -- epoch ms
    duration_ms INTEGER NOT NULL      -- cached for stats
);

CREATE INDEX IF NOT EXISTS idx_step_logs_workflow ON workflow_step_logs(workflow_id, step_index);
CREATE INDEX IF NOT EXISTS idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);

-- Workflow action plans: canonical plan JSON + hash for explainability
CREATE TABLE IF NOT EXISTS workflow_action_plans (
    workflow_id TEXT PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
    plan_id TEXT NOT NULL,
    plan_hash TEXT NOT NULL,
    plan_json TEXT NOT NULL,          -- canonical JSON
    created_at INTEGER NOT NULL       -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_action_plans_hash ON workflow_action_plans(plan_hash);

-- Prepared plans: plan previews awaiting commit
CREATE TABLE IF NOT EXISTS prepared_plans (
    plan_id TEXT PRIMARY KEY,
    plan_hash TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    action_kind TEXT NOT NULL,
    pane_id INTEGER,
    pane_uuid TEXT,
    params_json TEXT,
    plan_json TEXT NOT NULL,          -- redacted plan JSON for preview
    requires_approval INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,      -- epoch ms
    expires_at INTEGER NOT NULL,      -- epoch ms
    consumed_at INTEGER               -- epoch ms when commit was attempted
);

CREATE INDEX IF NOT EXISTS idx_prepared_plans_hash ON prepared_plans(plan_hash);
CREATE INDEX IF NOT EXISTS idx_prepared_plans_workspace ON prepared_plans(workspace_id);
CREATE INDEX IF NOT EXISTS idx_prepared_plans_expires ON prepared_plans(expires_at)
    WHERE consumed_at IS NULL;

-- Audit actions: policy decisions and outcomes
CREATE TABLE IF NOT EXISTS audit_actions (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,               -- epoch ms
    actor_kind TEXT NOT NULL,          -- human, robot, mcp, workflow
    actor_id TEXT,                     -- optional (workflow execution id, MCP client id)
    correlation_id TEXT,              -- optional chain/correlation identifier
    pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
    domain TEXT,
    action_kind TEXT NOT NULL,         -- send_text, workflow_run, etc.
    policy_decision TEXT NOT NULL,     -- allow, deny, require_approval
    decision_reason TEXT,
    rule_id TEXT,                      -- policy rule id if any
    input_summary TEXT,                -- redacted summary of input
    verification_summary TEXT,         -- redacted summary of verification
    decision_context TEXT,             -- JSON: decision context
    result TEXT NOT NULL               -- success, denied, failed, timeout
);

CREATE INDEX IF NOT EXISTS idx_audit_actions_ts ON audit_actions(ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_pane ON audit_actions(pane_id, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_actor ON audit_actions(actor_kind, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_action ON audit_actions(action_kind, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_decision ON audit_actions(policy_decision, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_correlation ON audit_actions(correlation_id);

-- Undo metadata for audit actions
CREATE TABLE IF NOT EXISTS action_undo (
    audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE,
    undoable INTEGER NOT NULL DEFAULT 0,
    undo_strategy TEXT NOT NULL,       -- none|manual|workflow_abort|pane_close|custom
    undo_hint TEXT,                    -- redacted guidance for humans
    undo_payload TEXT,                 -- JSON for executor (redacted)
    undone_at INTEGER,
    undone_by TEXT
);

CREATE INDEX IF NOT EXISTS idx_action_undo_undoable ON action_undo(undoable) WHERE undoable = 1;

-- Approval tokens: allow-once approvals scoped to actions
CREATE TABLE IF NOT EXISTS approval_tokens (
    id INTEGER PRIMARY KEY,
    code_hash TEXT NOT NULL,           -- sha256 hash of allow-once code
    created_at INTEGER NOT NULL,       -- epoch ms
    expires_at INTEGER NOT NULL,       -- epoch ms
    used_at INTEGER,                   -- epoch ms when consumed
    workspace_id TEXT NOT NULL,        -- workspace scope
    action_kind TEXT NOT NULL,         -- send_text, workflow_run, etc.
    pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
    action_fingerprint TEXT NOT NULL,  -- normalized action fingerprint
    plan_hash TEXT,                    -- optional sha256 hash of bound ActionPlan
    plan_version INTEGER,             -- optional plan schema version
    risk_summary TEXT                  -- optional human-readable risk description
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_approval_tokens_hash ON approval_tokens(code_hash);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_workspace ON approval_tokens(workspace_id, action_kind);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_pane ON approval_tokens(pane_id);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_expires ON approval_tokens(expires_at);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_unused ON approval_tokens(used_at) WHERE used_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_approval_tokens_fingerprint ON approval_tokens(action_fingerprint);

-- Accounts: mirrors caut usage data for failover selection
-- Supports: account selection policy, usage tracking
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY,
    account_id TEXT NOT NULL,          -- stable identifier (from caut or hash)
    service TEXT NOT NULL,             -- openai, anthropic, google, etc.
    name TEXT,                         -- display name
    percent_remaining REAL NOT NULL,   -- 0.0-100.0
    reset_at TEXT,                     -- ISO8601 or epoch string
    tokens_used INTEGER,
    tokens_remaining INTEGER,
    tokens_limit INTEGER,
    last_refreshed_at INTEGER NOT NULL, -- epoch ms
    last_used_at INTEGER,              -- epoch ms when used for failover
    created_at INTEGER NOT NULL,       -- epoch ms
    updated_at INTEGER NOT NULL        -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_service_account ON accounts(service, account_id);
CREATE INDEX IF NOT EXISTS idx_accounts_service ON accounts(service);
CREATE INDEX IF NOT EXISTS idx_accounts_percent ON accounts(service, percent_remaining DESC);
CREATE INDEX IF NOT EXISTS idx_accounts_last_used ON accounts(service, last_used_at);

-- Limit windows: forward-looking usage/rate-limit reset ledger
-- Supports: capacity forecasting, scheduling decline hooks, economic breaker
CREATE TABLE IF NOT EXISTS limit_windows (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    account_id TEXT NOT NULL,
    account_db_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    account_known INTEGER NOT NULL DEFAULT 0,
    agent_type TEXT,
    rule_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    limited_at INTEGER NOT NULL,
    reset_at INTEGER,
    reset_source TEXT NOT NULL,
    reset_text TEXT,
    conservative_ttl_ms INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1,
    metadata TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK(account_known IN (0, 1)),
    CHECK(reset_source IN ('absolute', 'retry_after', 'unknown_ttl')),
    CHECK(seen_count >= 1),
    UNIQUE(pane_id, service, account_id)
);

CREATE INDEX IF NOT EXISTS idx_limit_windows_pane_account
    ON limit_windows(pane_id, service, account_id);
CREATE INDEX IF NOT EXISTS idx_limit_windows_service_reset
    ON limit_windows(service, reset_at);
CREATE INDEX IF NOT EXISTS idx_limit_windows_last_seen
    ON limit_windows(last_seen_at);

-- Pane reservations: exclusive workflow locks on panes
-- Only one active reservation per pane; auto-expire on TTL
CREATE TABLE IF NOT EXISTS pane_reservations (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    owner_kind TEXT NOT NULL,          -- workflow, agent, manual
    owner_id TEXT NOT NULL,            -- workflow ID or agent name
    reason TEXT,                       -- human-readable reason
    created_at INTEGER NOT NULL,       -- epoch ms
    expires_at INTEGER NOT NULL,       -- epoch ms (created_at + TTL)
    released_at INTEGER,              -- epoch ms when released (NULL if active)
    status TEXT NOT NULL DEFAULT 'active'  -- active | released
);

CREATE INDEX IF NOT EXISTS idx_reservations_pane_status ON pane_reservations(pane_id, status);
CREATE INDEX IF NOT EXISTS idx_reservations_status ON pane_reservations(status);
CREATE INDEX IF NOT EXISTS idx_reservations_expires ON pane_reservations(expires_at) WHERE status = 'active';

-- FTS index state: track index version and per-pane progress for incremental sync
-- Enables efficient recovery without full reindex on restart
CREATE TABLE IF NOT EXISTS fts_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),  -- singleton row
    index_version INTEGER NOT NULL DEFAULT 1,
    last_full_rebuild_at INTEGER,           -- epoch ms
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Per-pane FTS indexing progress for batched rebuild
CREATE TABLE IF NOT EXISTS fts_pane_progress (
    pane_id INTEGER PRIMARY KEY REFERENCES panes(pane_id) ON DELETE CASCADE,
    last_indexed_seq INTEGER NOT NULL DEFAULT 0,
    indexed_count INTEGER NOT NULL DEFAULT 0,
    last_indexed_at INTEGER NOT NULL
);

-- Config: key-value settings
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,              -- JSON value
    updated_at INTEGER NOT NULL       -- epoch ms
);

-- Saved searches: persisted query definitions for reuse/scheduling
CREATE TABLE IF NOT EXISTS saved_searches (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    query TEXT NOT NULL,
    pane_id INTEGER,
    "limit" INTEGER NOT NULL DEFAULT 50,
    since_mode TEXT NOT NULL DEFAULT 'last_run',
    since_ms INTEGER,
    schedule_interval_ms INTEGER,
    enabled INTEGER NOT NULL DEFAULT 0,
    last_run_at INTEGER,
    last_result_count INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_saved_searches_enabled ON saved_searches(enabled);
CREATE INDEX IF NOT EXISTS idx_saved_searches_last_run ON saved_searches(last_run_at);

-- Maintenance log: system events and metrics
CREATE TABLE IF NOT EXISTS maintenance_log (
    id INTEGER PRIMARY KEY,
    event_type TEXT NOT NULL,         -- startup, shutdown, vacuum, retention_cleanup, error
    message TEXT,
    metadata TEXT,                    -- JSON: additional context
    timestamp INTEGER NOT NULL        -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_maintenance_timestamp ON maintenance_log(timestamp);

-- Secret scan reports: incremental scan checkpoints + report payloads
CREATE TABLE IF NOT EXISTS secret_scan_reports (
    id INTEGER PRIMARY KEY,
    scope_hash TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    report_version INTEGER NOT NULL,
    last_segment_id INTEGER,
    report_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_secret_scan_reports_scope
    ON secret_scan_reports(scope_hash, created_at);

-- Usage metrics: analytics data model for token/cost/API tracking
CREATE TABLE IF NOT EXISTS usage_metrics (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,          -- epoch ms
    metric_type TEXT NOT NULL,           -- token_usage, api_cost, api_call, rate_limit_hit, workflow_cost, session_duration
    pane_id INTEGER,                     -- NULL for global metrics
    agent_type TEXT,                     -- codex, claude_code, gemini, NULL
    account_id TEXT,                     -- caut account reference
    workflow_id TEXT,                    -- workflow execution reference
    count INTEGER,                       -- for countable metrics
    amount REAL,                         -- for costs (USD)
    tokens INTEGER,                      -- for token counts
    metadata TEXT,                       -- JSON for extensibility
    created_at INTEGER NOT NULL          -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_usage_metrics_timestamp ON usage_metrics(timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_type_ts ON usage_metrics(metric_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_agent_ts ON usage_metrics(agent_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_account_ts ON usage_metrics(account_id, timestamp);

-- Notification history: persistent log of all sent notifications
CREATE TABLE IF NOT EXISTS notification_history (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,          -- epoch ms when notification was created
    event_id INTEGER,                    -- optional FK to events(id)
    channel TEXT NOT NULL,               -- webhook, desktop, slack, etc.
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    severity TEXT NOT NULL,              -- info, warning, error, critical
    status TEXT NOT NULL DEFAULT 'pending', -- pending, sent, failed, throttled
    error_message TEXT,                  -- error details on failure
    acknowledged_at INTEGER,             -- epoch ms
    acknowledged_by TEXT,
    action_taken TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    metadata TEXT,                       -- JSON blob for channel-specific data
    created_at INTEGER NOT NULL          -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_notification_history_timestamp ON notification_history(timestamp);
CREATE INDEX IF NOT EXISTS idx_notification_history_status ON notification_history(status);
CREATE INDEX IF NOT EXISTS idx_notification_history_event ON notification_history(event_id);
CREATE INDEX IF NOT EXISTS idx_notification_history_channel_ts ON notification_history(channel, timestamp);

-- Pane bookmarks: named aliases with optional tags for fast pane access
CREATE TABLE IF NOT EXISTS pane_bookmarks (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL,
    alias TEXT NOT NULL UNIQUE,
    tags TEXT,                            -- JSON array of tag strings
    description TEXT,
    created_at INTEGER NOT NULL,          -- epoch ms
    updated_at INTEGER NOT NULL           -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_pane_id ON pane_bookmarks(pane_id);
CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_alias ON pane_bookmarks(alias);

-- Mux sessions: top-level session tracking (one per watcher invocation)
-- FT_MUX_SESSIONS_SCHEMA_BEGIN
CREATE TABLE IF NOT EXISTS mux_sessions (
    session_id TEXT PRIMARY KEY,           -- UUID v7 for time-ordering
    created_at INTEGER NOT NULL,           -- epoch ms
    last_checkpoint_at INTEGER,            -- epoch ms
    shutdown_clean INTEGER NOT NULL DEFAULT 0,  -- 1 = graceful, 0 = crash/power loss
    topology_json TEXT NOT NULL,           -- serialized tab/split tree
    window_metadata_json TEXT,             -- window size, title, position
    ft_version TEXT NOT NULL,              -- binary version at creation
    host_id TEXT,                          -- versioned app-scoped machine fence + hostname + boot + process domain
    owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0),
    owner_process_start INTEGER CHECK(owner_process_start IS NULL OR owner_process_start > 0),
    owner_heartbeat_at INTEGER CHECK(owner_heartbeat_at IS NULL OR owner_heartbeat_at >= 0),
    recovery_acknowledged_at INTEGER
        CHECK(recovery_acknowledged_at IS NULL OR recovery_acknowledged_at >= 0),
    clean_checkpoint_id INTEGER REFERENCES session_checkpoints(id) ON DELETE SET NULL,
                                            -- exact receipt authorizing shutdown_clean = 1
    CHECK(
        (owner_pid IS NULL
            AND owner_process_start IS NULL
            AND owner_heartbeat_at IS NULL)
        OR (owner_pid IS NOT NULL
            AND owner_process_start IS NOT NULL
            AND owner_heartbeat_at IS NOT NULL
            AND host_id IS NOT NULL)
    )
);
-- FT_MUX_SESSIONS_SCHEMA_END

-- Session checkpoints: individual checkpoint snapshots (many per session)
CREATE TABLE IF NOT EXISTS session_checkpoints (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    checkpoint_at INTEGER NOT NULL,        -- epoch ms
    checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
    state_hash TEXT NOT NULL,              -- versioned SHA-256 consistency witness; not authentication
    pane_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,          -- historical pane-state JSON byte estimate
    metadata_json TEXT,                    -- trigger reason / operator metadata
    checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
        CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
    topology_json TEXT,                    -- exact topology for this snapshot; NULL for intents/receipts/legacy rows
    restore_intent_checkpoint_id INTEGER
        REFERENCES session_checkpoints(id) ON DELETE CASCADE,
    CHECK(checkpoint_role = 'restore_receipt' OR restore_intent_checkpoint_id IS NULL)
);

CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_latest
    ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_causal
    ON session_checkpoints(session_id, checkpoint_role, id DESC);
CREATE INDEX IF NOT EXISTS idx_checkpoints_global_latest
    ON session_checkpoints(checkpoint_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_checkpoints_global_snapshot_latest
    ON session_checkpoints(checkpoint_at DESC, id DESC)
    WHERE checkpoint_role = 'snapshot';
CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_restore_intent_outcome
    ON session_checkpoints(restore_intent_checkpoint_id)
    WHERE restore_intent_checkpoint_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mux_sessions_clean_checkpoint
    ON mux_sessions(clean_checkpoint_id);
CREATE INDEX IF NOT EXISTS idx_mux_sessions_recovery_lifecycle
    ON mux_sessions(
        shutdown_clean, recovery_acknowledged_at,
        last_checkpoint_at DESC, session_id
    );

-- Restore-attempt lifecycle: authoritative settlement state independent of
-- whichever checkpoint happens to be latest after an interrupted attempt.
CREATE TABLE IF NOT EXISTS restore_attempt_lifecycle (
    intent_checkpoint_id INTEGER PRIMARY KEY
        REFERENCES session_checkpoints(id)
        ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
    session_id TEXT NOT NULL
        REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    -- Durable causal identity. Deliberately not an FK: resolved attempts must
    -- not pin a potentially large source snapshot forever; unresolved source
    -- deletion is blocked by the checkpoint-prune authority path while this
    -- identity remains available for reconciliation even after resolution.
    source_checkpoint_id INTEGER NOT NULL,
    outcome_checkpoint_id INTEGER
        REFERENCES session_checkpoints(id) ON DELETE SET NULL,
    status TEXT NOT NULL
        CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required')),
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    CHECK(intent_checkpoint_id <> source_checkpoint_id),
    CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> intent_checkpoint_id),
    CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> source_checkpoint_id),
    CHECK(created_at >= 0),
    CHECK(resolved_at IS NULL OR resolved_at >= created_at),
    CHECK(
        (status = 'intent'
            AND outcome_checkpoint_id IS NULL
            AND resolved_at IS NULL)
        OR (status = 'outcome_complete'
            AND outcome_checkpoint_id IS NOT NULL
            AND resolved_at IS NULL)
        OR (status = 'reconciliation_required'
            AND resolved_at IS NULL)
        OR (status = 'resolved'
            AND resolved_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_session_status
    ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_outcome
    ON restore_attempt_lifecycle(outcome_checkpoint_id)
    WHERE outcome_checkpoint_id IS NOT NULL;

-- Mux pane state: per-pane state snapshot, linked to a checkpoint
CREATE TABLE IF NOT EXISTS mux_pane_state (
    id INTEGER PRIMARY KEY,
    checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
    pane_id INTEGER NOT NULL,              -- WezTerm pane ID at capture time
    cwd TEXT,
    command TEXT,                           -- best-effort process name
    env_json TEXT,                          -- selected env vars (redacted)
    terminal_state_json TEXT NOT NULL,      -- cursor pos, attributes, alt-screen, scrollback ref
    agent_metadata_json TEXT,               -- agent type, session ID, state
    scrollback_checkpoint_seq INTEGER,      -- links to output_segments.seq for replay
    last_output_at INTEGER                 -- epoch ms of last captured output
);

CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);

-- FT_SESSION_RECOVERY_USABILITY_V44_BEGIN
-- Derived recovery usability authority (ft-0yuxe.6).
--
-- `dirty` is deliberately fail-closed: retention may reconcile only a fixed
-- batch per transaction and cannot delete an unacknowledged crash session
-- while its newest snapshot has not been checked by the canonical restore
-- loader.  The validation row is invalidated transactionally by every source
-- mutation that can change restore semantics.
CREATE TABLE IF NOT EXISTS session_recovery_usability (
    session_id TEXT PRIMARY KEY
        CHECK(typeof(session_id) = 'text'
            AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND 256)
        REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    authority_version INTEGER NOT NULL DEFAULT 1
        CHECK(typeof(authority_version) = 'integer' AND authority_version = 1),
    state TEXT NOT NULL DEFAULT 'dirty'
        CHECK(state IN ('dirty','usable','unusable')),
    validated_checkpoint_id INTEGER,
    dirty_generation INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(dirty_generation) = 'integer' AND dirty_generation >= 0),
    CHECK(
        (state = 'usable'
            AND typeof(validated_checkpoint_id) = 'integer'
            AND validated_checkpoint_id > 0)
        OR (state IN ('dirty','unusable') AND validated_checkpoint_id IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_session_recovery_usability_state
    ON session_recovery_usability(validated_checkpoint_id DESC, session_id ASC)
    WHERE state = 'usable';
CREATE INDEX IF NOT EXISTS idx_session_recovery_usability_dirty
    ON session_recovery_usability(dirty_generation ASC, session_id ASC)
    WHERE state = 'dirty';

-- One durable cursor makes a long prefix of live/foreign/unknown owners
-- restart-safe.  The first source mutation after validation advances
-- mutation_generation, marks the session dirty, and resets the cursor in the
-- same SQLite transaction; later pane mutations coalesce while it remains
-- dirty. Integer exhaustion aborts rather than permitting authority reuse.
CREATE TABLE IF NOT EXISTS session_recovery_selection (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    mutation_generation INTEGER NOT NULL
        CHECK(typeof(mutation_generation) = 'integer' AND mutation_generation >= 0),
    population_after_rowid INTEGER
        CHECK(population_after_rowid IS NULL
            OR typeof(population_after_rowid) = 'integer'),
    population_complete INTEGER NOT NULL DEFAULT 1
        CHECK(typeof(population_complete) = 'integer'
            AND population_complete IN (0, 1)),
    scan_generation INTEGER NOT NULL
        CHECK(typeof(scan_generation) = 'integer' AND scan_generation >= 0),
    scan_after_checkpoint_id INTEGER,
    scan_after_session_id TEXT,
    protected_session_id TEXT,
    protected_checkpoint_id INTEGER,
    scan_complete INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(scan_complete) = 'integer' AND scan_complete IN (0, 1)),
    CHECK(
        (scan_after_checkpoint_id IS NULL AND scan_after_session_id IS NULL)
        OR (typeof(scan_after_checkpoint_id) = 'integer'
            AND scan_after_checkpoint_id > 0
            AND typeof(scan_after_session_id) = 'text'
            AND length(CAST(scan_after_session_id AS BLOB)) BETWEEN 1 AND 256)
    ),
    CHECK(
        (protected_session_id IS NULL AND protected_checkpoint_id IS NULL)
        OR (typeof(protected_session_id) = 'text'
            AND length(CAST(protected_session_id AS BLOB)) BETWEEN 1 AND 256
            AND typeof(protected_checkpoint_id) = 'integer'
            AND protected_checkpoint_id > 0)
    )
);

INSERT OR IGNORE INTO session_recovery_selection (
    singleton, mutation_generation, population_complete,
    scan_generation, scan_complete
) VALUES (1, 0, 1, 0, 0);

CREATE TRIGGER IF NOT EXISTS mux_sessions_recovery_usability_ai
AFTER INSERT ON mux_sessions BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    INSERT INTO session_recovery_usability (
        session_id, state, validated_checkpoint_id, dirty_generation
    ) VALUES (
        new.session_id, 'dirty', NULL,
        (SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1)
    ) ON CONFLICT(session_id) DO UPDATE SET
        state = 'dirty',
        validated_checkpoint_id = NULL,
        dirty_generation = excluded.dirty_generation;
END;

CREATE TRIGGER IF NOT EXISTS mux_sessions_recovery_usability_bu
BEFORE UPDATE OF shutdown_clean, recovery_acknowledged_at,
                 host_id, owner_pid, owner_process_start ON mux_sessions
WHEN new.shutdown_clean IS NOT old.shutdown_clean
  OR new.recovery_acknowledged_at IS NOT old.recovery_acknowledged_at
  OR new.host_id IS NOT old.host_id
  OR new.owner_pid IS NOT old.owner_pid
  OR new.owner_process_start IS NOT old.owner_process_start
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
END;

CREATE TRIGGER IF NOT EXISTS mux_sessions_recovery_usability_bd
BEFORE DELETE ON mux_sessions BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    DELETE FROM session_recovery_usability
    WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_recovery_usability_ai
AFTER INSERT ON session_checkpoints
WHEN EXISTS (
    SELECT 1 FROM session_recovery_usability
    WHERE session_id = new.session_id AND state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty',
        validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE session_id = new.session_id AND state <> 'dirty';
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_recovery_usability_au
AFTER UPDATE ON session_checkpoints
WHEN EXISTS (
    SELECT 1 FROM session_recovery_usability
    WHERE session_id IN (old.session_id, new.session_id)
      AND state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty', validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE session_id IN (old.session_id, new.session_id)
      AND state <> 'dirty';
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_recovery_usability_bd
BEFORE DELETE ON session_checkpoints
WHEN EXISTS (
    SELECT 1 FROM session_recovery_usability
    WHERE session_id = old.session_id AND state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty', validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE session_id = old.session_id AND state <> 'dirty';
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_recovery_usability_ai
AFTER INSERT ON mux_pane_state
WHEN EXISTS (
    SELECT 1
    FROM session_recovery_usability AS usability
    INNER JOIN session_checkpoints AS checkpoint
      ON checkpoint.session_id = usability.session_id
    WHERE checkpoint.id = new.checkpoint_id
      AND usability.state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty', validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE state <> 'dirty' AND session_id = (
        SELECT session_id FROM session_checkpoints WHERE id = new.checkpoint_id
    );
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_recovery_usability_au
AFTER UPDATE ON mux_pane_state
WHEN EXISTS (
    SELECT 1
    FROM session_recovery_usability AS usability
    INNER JOIN session_checkpoints AS checkpoint
      ON checkpoint.session_id = usability.session_id
    WHERE checkpoint.id IN (old.checkpoint_id, new.checkpoint_id)
      AND usability.state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty', validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE state <> 'dirty' AND session_id IN (
        SELECT session_id FROM session_checkpoints
        WHERE id IN (old.checkpoint_id, new.checkpoint_id)
    );
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_recovery_usability_bd
BEFORE DELETE ON mux_pane_state
WHEN EXISTS (
    SELECT 1
    FROM session_recovery_usability AS usability
    INNER JOIN session_checkpoints AS checkpoint
      ON checkpoint.session_id = usability.session_id
    WHERE checkpoint.id = old.checkpoint_id
      AND usability.state <> 'dirty'
)
BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1;
    UPDATE session_recovery_usability
    SET state = 'dirty', validated_checkpoint_id = NULL,
        dirty_generation = (
            SELECT mutation_generation FROM session_recovery_selection WHERE singleton = 1
        )
    WHERE state <> 'dirty' AND session_id = (
        SELECT session_id FROM session_checkpoints WHERE id = old.checkpoint_id
    );
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_recovery_usability_ai
AFTER INSERT ON restore_attempt_lifecycle BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM session_recovery_usability
          WHERE session_id = new.session_id
      );
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_recovery_usability_au
AFTER UPDATE ON restore_attempt_lifecycle BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM session_recovery_usability
          WHERE session_id IN (old.session_id, new.session_id)
      );
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_recovery_usability_bd
BEFORE DELETE ON restore_attempt_lifecycle BEGIN
    UPDATE session_recovery_selection
    SET mutation_generation = CASE
            WHEN mutation_generation < 9223372036854775807
            THEN mutation_generation + 1
            ELSE RAISE(ABORT, 'recovery usability generation exhausted')
        END,
        scan_generation = 0,
        scan_after_checkpoint_id = NULL,
        scan_after_session_id = NULL,
        protected_session_id = NULL,
        protected_checkpoint_id = NULL,
        scan_complete = 0
    WHERE singleton = 1
      AND EXISTS (
          SELECT 1 FROM session_recovery_usability
          WHERE session_id = old.session_id
      );
END;
-- FT_SESSION_RECOVERY_USABILITY_V44_END

-- FT_SESSION_RETAINED_SIZE_V40_BEGIN
-- Exact logical retained-payload bytes for session size retention (ft-0yuxe.3).
--
-- Contract: each non-NULL INTEGER field contributes 8 bytes; each TEXT/BLOB
-- field contributes its exact encoded byte length; NULL contributes 0. Every
-- stored field in mux_sessions, session_checkpoints, mux_pane_state, and
-- restore_attempt_lifecycle is charged exactly once. SQLite record framing,
-- b-tree pages, indexes, freelist pages, and WAL bytes are physical storage and
-- deliberately outside this logical retained-payload budget.
CREATE VIEW IF NOT EXISTS session_retained_size_recomputed AS
SELECT
    s.session_id,
    length(CAST(s.session_id AS BLOB))
        + 8
        + CASE WHEN s.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
        + 8
        + length(CAST(s.topology_json AS BLOB))
        + COALESCE(length(CAST(s.window_metadata_json AS BLOB)), 0)
        + length(CAST(s.ft_version AS BLOB))
        + COALESCE(length(CAST(s.host_id AS BLOB)), 0)
        + CASE WHEN s.owner_pid IS NULL THEN 0 ELSE 8 END
        + CASE WHEN s.owner_process_start IS NULL THEN 0 ELSE 8 END
        + CASE WHEN s.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
        + CASE WHEN s.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
        + CASE WHEN s.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
        AS session_row_bytes,
    COALESCE((
        SELECT SUM(
            8
            + length(CAST(c.session_id AS BLOB))
            + 8
            + length(CAST(c.checkpoint_type AS BLOB))
            + length(CAST(c.state_hash AS BLOB))
            + 8
            + 8
            + COALESCE(length(CAST(c.metadata_json AS BLOB)), 0)
            + length(CAST(c.checkpoint_role AS BLOB))
            + COALESCE(length(CAST(c.topology_json AS BLOB)), 0)
            + CASE WHEN c.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
        )
        FROM session_checkpoints c
        WHERE c.session_id = s.session_id
    ), 0) AS checkpoint_row_bytes,
    COALESCE((
        SELECT SUM(
            8 + 8 + 8
            + COALESCE(length(CAST(p.cwd AS BLOB)), 0)
            + COALESCE(length(CAST(p.command AS BLOB)), 0)
            + COALESCE(length(CAST(p.env_json AS BLOB)), 0)
            + length(CAST(p.terminal_state_json AS BLOB))
            + COALESCE(length(CAST(p.agent_metadata_json AS BLOB)), 0)
            + CASE WHEN p.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
            + CASE WHEN p.last_output_at IS NULL THEN 0 ELSE 8 END
        )
        FROM mux_pane_state p
        INNER JOIN session_checkpoints c ON c.id = p.checkpoint_id
        WHERE c.session_id = s.session_id
    ), 0) AS pane_state_row_bytes,
    COALESCE((
        SELECT SUM(
            8
            + length(CAST(r.session_id AS BLOB))
            + 8
            + CASE WHEN r.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
            + length(CAST(r.status AS BLOB))
            + 8
            + CASE WHEN r.resolved_at IS NULL THEN 0 ELSE 8 END
        )
        FROM restore_attempt_lifecycle r
        WHERE r.session_id = s.session_id
    ), 0) AS restore_lifecycle_row_bytes
FROM mux_sessions s;

CREATE TABLE IF NOT EXISTS session_retained_size (
    session_id TEXT PRIMARY KEY
        REFERENCES mux_sessions(session_id) ON DELETE NO ACTION
        DEFERRABLE INITIALLY DEFERRED,
    session_row_bytes INTEGER NOT NULL
        CHECK(typeof(session_row_bytes) = 'integer' AND session_row_bytes >= 0),
    checkpoint_row_bytes INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(checkpoint_row_bytes) = 'integer' AND checkpoint_row_bytes >= 0),
    pane_state_row_bytes INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(pane_state_row_bytes) = 'integer' AND pane_state_row_bytes >= 0),
    restore_lifecycle_row_bytes INTEGER NOT NULL DEFAULT 0
        CHECK(typeof(restore_lifecycle_row_bytes) = 'integer' AND
              restore_lifecycle_row_bytes >= 0),
    retained_bytes INTEGER GENERATED ALWAYS AS (
        session_row_bytes + checkpoint_row_bytes + pane_state_row_bytes
            + restore_lifecycle_row_bytes
    ) STORED
        CHECK(typeof(retained_bytes) = 'integer' AND retained_bytes >= 0)
);

INSERT OR IGNORE INTO session_retained_size (
    session_id, session_row_bytes, checkpoint_row_bytes,
    pane_state_row_bytes, restore_lifecycle_row_bytes
)
SELECT session_id, session_row_bytes, checkpoint_row_bytes,
       pane_state_row_bytes, restore_lifecycle_row_bytes
FROM session_retained_size_recomputed;

CREATE TRIGGER IF NOT EXISTS session_retained_size_bi
BEFORE INSERT ON session_retained_size
WHEN NOT EXISTS (
    SELECT 1 FROM mux_sessions WHERE session_id = new.session_id
) BEGIN
    SELECT RAISE(ABORT, 'session retained-size row requires a persisted session');
END;

CREATE TRIGGER IF NOT EXISTS session_retained_size_bd
BEFORE DELETE ON session_retained_size
WHEN EXISTS (
    SELECT 1 FROM mux_sessions WHERE session_id = old.session_id
) BEGIN
    SELECT RAISE(ABORT, 'live session retained-size authority is permanent');
END;

CREATE TRIGGER IF NOT EXISTS session_retained_size_session_id_bu
BEFORE UPDATE OF session_id ON session_retained_size
WHEN new.session_id != old.session_id BEGIN
    SELECT RAISE(ABORT, 'session retained-size identity is immutable');
END;

CREATE TRIGGER IF NOT EXISTS mux_sessions_retained_size_ai
AFTER INSERT ON mux_sessions BEGIN
    SELECT CASE WHEN NOT (
        (new.owner_pid IS NULL
            AND new.owner_process_start IS NULL
            AND new.owner_heartbeat_at IS NULL)
        OR (new.owner_pid IS NOT NULL
            AND new.owner_process_start IS NOT NULL
            AND new.owner_heartbeat_at IS NOT NULL
            AND new.host_id IS NOT NULL)
    ) THEN RAISE(ABORT, 'invalid session owner lifecycle tuple') END;
    INSERT INTO session_retained_size (session_id, session_row_bytes)
    VALUES (
        new.session_id,
        length(CAST(new.session_id AS BLOB))
            + 8
            + CASE WHEN new.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
            + 8
            + length(CAST(new.topology_json AS BLOB))
            + COALESCE(length(CAST(new.window_metadata_json AS BLOB)), 0)
            + length(CAST(new.ft_version AS BLOB))
            + COALESCE(length(CAST(new.host_id AS BLOB)), 0)
            + CASE WHEN new.owner_pid IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_process_start IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
    );
END;

CREATE TRIGGER IF NOT EXISTS mux_sessions_retained_size_au
AFTER UPDATE ON mux_sessions BEGIN
    SELECT CASE WHEN NOT (
        (new.owner_pid IS NULL
            AND new.owner_process_start IS NULL
            AND new.owner_heartbeat_at IS NULL)
        OR (new.owner_pid IS NOT NULL
            AND new.owner_process_start IS NOT NULL
            AND new.owner_heartbeat_at IS NOT NULL
            AND new.host_id IS NOT NULL)
    ) THEN RAISE(ABORT, 'invalid session owner lifecycle tuple') END;
    SELECT CASE WHEN new.session_id != old.session_id THEN
        RAISE(ABORT, 'session retained-size identity is immutable')
    END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during session update') END;
    SELECT CASE WHEN session_row_bytes != (
        length(CAST(old.session_id AS BLOB))
            + 8
            + CASE WHEN old.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
            + 8
            + length(CAST(old.topology_json AS BLOB))
            + COALESCE(length(CAST(old.window_metadata_json AS BLOB)), 0)
            + length(CAST(old.ft_version AS BLOB))
            + COALESCE(length(CAST(old.host_id AS BLOB)), 0)
            + CASE WHEN old.owner_pid IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.owner_process_start IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size authority drift during session update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    SELECT CASE WHEN (
        length(CAST(new.session_id AS BLOB))
            + 8
            + CASE WHEN new.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
            + 8
            + length(CAST(new.topology_json AS BLOB))
            + COALESCE(length(CAST(new.window_metadata_json AS BLOB)), 0)
            + length(CAST(new.ft_version AS BLOB))
            + COALESCE(length(CAST(new.host_id AS BLOB)), 0)
            + CASE WHEN new.owner_pid IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_process_start IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) > session_row_bytes AND retained_bytes > 9223372036854775807 - ((
        length(CAST(new.session_id AS BLOB))
            + 8
            + CASE WHEN new.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
            + 8
            + length(CAST(new.topology_json AS BLOB))
            + COALESCE(length(CAST(new.window_metadata_json AS BLOB)), 0)
            + length(CAST(new.ft_version AS BLOB))
            + COALESCE(length(CAST(new.host_id AS BLOB)), 0)
            + CASE WHEN new.owner_pid IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_process_start IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) - session_row_bytes)
    THEN RAISE(ABORT, 'session retained-size overflow during session update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    UPDATE session_retained_size
    SET session_row_bytes =
        length(CAST(new.session_id AS BLOB))
            + 8
            + CASE WHEN new.last_checkpoint_at IS NULL THEN 0 ELSE 8 END
            + 8
            + length(CAST(new.topology_json AS BLOB))
            + COALESCE(length(CAST(new.window_metadata_json AS BLOB)), 0)
            + length(CAST(new.ft_version AS BLOB))
            + COALESCE(length(CAST(new.host_id AS BLOB)), 0)
            + CASE WHEN new.owner_pid IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_process_start IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.owner_heartbeat_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.recovery_acknowledged_at IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.clean_checkpoint_id IS NULL THEN 0 ELSE 8 END
    WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS mux_sessions_retained_size_ad
AFTER DELETE ON mux_sessions BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during session delete') END;
    DELETE FROM session_retained_size WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_retained_size_ai
AFTER INSERT ON session_checkpoints BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = new.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during checkpoint insert') END;
    SELECT CASE WHEN retained_bytes > 9223372036854775807 - (
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + length(CAST(new.checkpoint_type AS BLOB))
        + length(CAST(new.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(new.metadata_json AS BLOB)), 0)
        + length(CAST(new.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(new.topology_json AS BLOB)), 0)
        + CASE WHEN new.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size overflow during checkpoint insert') END
    FROM session_retained_size WHERE session_id = new.session_id;
    UPDATE session_retained_size
    SET checkpoint_row_bytes = checkpoint_row_bytes
        + 8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + length(CAST(new.checkpoint_type AS BLOB))
        + length(CAST(new.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(new.metadata_json AS BLOB)), 0)
        + length(CAST(new.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(new.topology_json AS BLOB)), 0)
        + CASE WHEN new.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    WHERE session_id = new.session_id;
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_retained_size_au
AFTER UPDATE ON session_checkpoints BEGIN
    SELECT CASE WHEN new.id != old.id OR new.session_id != old.session_id THEN
        RAISE(ABORT, 'checkpoint retained-size identity is immutable')
    END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during checkpoint update') END;
    SELECT CASE WHEN checkpoint_row_bytes < (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + length(CAST(old.checkpoint_type AS BLOB))
        + length(CAST(old.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
        + length(CAST(old.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
        + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during checkpoint update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    SELECT CASE WHEN (
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + length(CAST(new.checkpoint_type AS BLOB))
        + length(CAST(new.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(new.metadata_json AS BLOB)), 0)
        + length(CAST(new.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(new.topology_json AS BLOB)), 0)
        + CASE WHEN new.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) > (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + length(CAST(old.checkpoint_type AS BLOB))
        + length(CAST(old.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
        + length(CAST(old.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
        + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) AND retained_bytes > 9223372036854775807 - ((
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + length(CAST(new.checkpoint_type AS BLOB))
        + length(CAST(new.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(new.metadata_json AS BLOB)), 0)
        + length(CAST(new.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(new.topology_json AS BLOB)), 0)
        + CASE WHEN new.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) - (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + length(CAST(old.checkpoint_type AS BLOB))
        + length(CAST(old.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
        + length(CAST(old.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
        + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    )) THEN RAISE(ABORT, 'session retained-size overflow during checkpoint update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    UPDATE session_retained_size
    SET checkpoint_row_bytes = checkpoint_row_bytes
        - (
            8
            + length(CAST(old.session_id AS BLOB))
            + 8
            + length(CAST(old.checkpoint_type AS BLOB))
            + length(CAST(old.state_hash AS BLOB))
            + 8
            + 8
            + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
            + length(CAST(old.checkpoint_role AS BLOB))
            + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
            + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
        )
        + (
            8
            + length(CAST(new.session_id AS BLOB))
            + 8
            + length(CAST(new.checkpoint_type AS BLOB))
            + length(CAST(new.state_hash AS BLOB))
            + 8
            + 8
            + COALESCE(length(CAST(new.metadata_json AS BLOB)), 0)
            + length(CAST(new.checkpoint_role AS BLOB))
            + COALESCE(length(CAST(new.topology_json AS BLOB)), 0)
            + CASE WHEN new.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
        )
    WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS session_checkpoints_retained_size_bd
BEFORE DELETE ON session_checkpoints BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1 FROM mux_sessions WHERE session_id = old.session_id
    ) AND NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during checkpoint delete') END;
    SELECT CASE WHEN checkpoint_row_bytes < (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + length(CAST(old.checkpoint_type AS BLOB))
        + length(CAST(old.state_hash AS BLOB))
        + 8
        + 8
        + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
        + length(CAST(old.checkpoint_role AS BLOB))
        + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
        + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during checkpoint delete') END
    FROM session_retained_size WHERE session_id = old.session_id;
    SELECT CASE WHEN pane_state_row_bytes < COALESCE((
        SELECT SUM(
            8 + 8 + 8
            + COALESCE(length(CAST(p.cwd AS BLOB)), 0)
            + COALESCE(length(CAST(p.command AS BLOB)), 0)
            + COALESCE(length(CAST(p.env_json AS BLOB)), 0)
            + length(CAST(p.terminal_state_json AS BLOB))
            + COALESCE(length(CAST(p.agent_metadata_json AS BLOB)), 0)
            + CASE WHEN p.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
            + CASE WHEN p.last_output_at IS NULL THEN 0 ELSE 8 END
        )
        FROM mux_pane_state p
        WHERE p.checkpoint_id = old.id
    ), 0) THEN RAISE(ABORT, 'session retained-size underflow during checkpoint pane cascade') END
    FROM session_retained_size WHERE session_id = old.session_id;
    UPDATE session_retained_size
    SET checkpoint_row_bytes = checkpoint_row_bytes - (
            8
            + length(CAST(old.session_id AS BLOB))
            + 8
            + length(CAST(old.checkpoint_type AS BLOB))
            + length(CAST(old.state_hash AS BLOB))
            + 8
            + 8
            + COALESCE(length(CAST(old.metadata_json AS BLOB)), 0)
            + length(CAST(old.checkpoint_role AS BLOB))
            + COALESCE(length(CAST(old.topology_json AS BLOB)), 0)
            + CASE WHEN old.restore_intent_checkpoint_id IS NULL THEN 0 ELSE 8 END
        ),
        pane_state_row_bytes = pane_state_row_bytes - COALESCE((
            SELECT SUM(
                8 + 8 + 8
                + COALESCE(length(CAST(p.cwd AS BLOB)), 0)
                + COALESCE(length(CAST(p.command AS BLOB)), 0)
                + COALESCE(length(CAST(p.env_json AS BLOB)), 0)
                + length(CAST(p.terminal_state_json AS BLOB))
                + COALESCE(length(CAST(p.agent_metadata_json AS BLOB)), 0)
                + CASE WHEN p.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
                + CASE WHEN p.last_output_at IS NULL THEN 0 ELSE 8 END
            )
            FROM mux_pane_state p
            WHERE p.checkpoint_id = old.id
        ), 0)
    WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_retained_size_ai
AFTER INSERT ON mux_pane_state BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1
        FROM session_checkpoints c
        INNER JOIN session_retained_size z ON z.session_id = c.session_id
        WHERE c.id = new.checkpoint_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during pane-state insert') END;
    SELECT CASE WHEN z.retained_bytes > 9223372036854775807 - (
        8 + 8 + 8
        + COALESCE(length(CAST(new.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(new.command AS BLOB)), 0)
        + COALESCE(length(CAST(new.env_json AS BLOB)), 0)
        + length(CAST(new.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(new.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN new.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN new.last_output_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size overflow during pane-state insert') END
    FROM session_checkpoints c
    INNER JOIN session_retained_size z ON z.session_id = c.session_id
    WHERE c.id = new.checkpoint_id;
    UPDATE session_retained_size
    SET pane_state_row_bytes = pane_state_row_bytes
        + 8 + 8 + 8
        + COALESCE(length(CAST(new.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(new.command AS BLOB)), 0)
        + COALESCE(length(CAST(new.env_json AS BLOB)), 0)
        + length(CAST(new.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(new.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN new.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN new.last_output_at IS NULL THEN 0 ELSE 8 END
    WHERE session_id = (
        SELECT session_id FROM session_checkpoints WHERE id = new.checkpoint_id
    );
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_retained_size_au
AFTER UPDATE ON mux_pane_state BEGIN
    SELECT CASE WHEN new.id != old.id OR new.checkpoint_id != old.checkpoint_id THEN
        RAISE(ABORT, 'pane-state retained-size identity is immutable')
    END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1
        FROM session_checkpoints c
        INNER JOIN session_retained_size z ON z.session_id = c.session_id
        WHERE c.id = old.checkpoint_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during pane-state update') END;
    SELECT CASE WHEN z.pane_state_row_bytes < (
        8 + 8 + 8
        + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(old.command AS BLOB)), 0)
        + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
        + length(CAST(old.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during pane-state update') END
    FROM session_checkpoints c
    INNER JOIN session_retained_size z ON z.session_id = c.session_id
    WHERE c.id = old.checkpoint_id;
    SELECT CASE WHEN (
        8 + 8 + 8
        + COALESCE(length(CAST(new.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(new.command AS BLOB)), 0)
        + COALESCE(length(CAST(new.env_json AS BLOB)), 0)
        + length(CAST(new.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(new.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN new.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN new.last_output_at IS NULL THEN 0 ELSE 8 END
    ) > (
        8 + 8 + 8
        + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(old.command AS BLOB)), 0)
        + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
        + length(CAST(old.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
    ) AND z.retained_bytes > 9223372036854775807 - ((
        8 + 8 + 8
        + COALESCE(length(CAST(new.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(new.command AS BLOB)), 0)
        + COALESCE(length(CAST(new.env_json AS BLOB)), 0)
        + length(CAST(new.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(new.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN new.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN new.last_output_at IS NULL THEN 0 ELSE 8 END
    ) - (
        8 + 8 + 8
        + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(old.command AS BLOB)), 0)
        + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
        + length(CAST(old.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
    )) THEN RAISE(ABORT, 'session retained-size overflow during pane-state update') END
    FROM session_checkpoints c
    INNER JOIN session_retained_size z ON z.session_id = c.session_id
    WHERE c.id = old.checkpoint_id;
    UPDATE session_retained_size
    SET pane_state_row_bytes = pane_state_row_bytes
        - (
            8 + 8 + 8
            + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
            + COALESCE(length(CAST(old.command AS BLOB)), 0)
            + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
            + length(CAST(old.terminal_state_json AS BLOB))
            + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
            + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
        )
        + (
            8 + 8 + 8
            + COALESCE(length(CAST(new.cwd AS BLOB)), 0)
            + COALESCE(length(CAST(new.command AS BLOB)), 0)
            + COALESCE(length(CAST(new.env_json AS BLOB)), 0)
            + length(CAST(new.terminal_state_json AS BLOB))
            + COALESCE(length(CAST(new.agent_metadata_json AS BLOB)), 0)
            + CASE WHEN new.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
            + CASE WHEN new.last_output_at IS NULL THEN 0 ELSE 8 END
        )
    WHERE session_id = (
        SELECT session_id FROM session_checkpoints WHERE id = old.checkpoint_id
    );
END;

CREATE TRIGGER IF NOT EXISTS mux_pane_state_retained_size_ad
AFTER DELETE ON mux_pane_state BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1
        FROM session_checkpoints c
        INNER JOIN mux_sessions s ON s.session_id = c.session_id
        WHERE c.id = old.checkpoint_id
    ) AND NOT EXISTS (
        SELECT 1
        FROM session_checkpoints c
        INNER JOIN session_retained_size z ON z.session_id = c.session_id
        WHERE c.id = old.checkpoint_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during pane-state delete') END;
    SELECT CASE WHEN z.pane_state_row_bytes < (
        8 + 8 + 8
        + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
        + COALESCE(length(CAST(old.command AS BLOB)), 0)
        + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
        + length(CAST(old.terminal_state_json AS BLOB))
        + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
        + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
        + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during pane-state delete') END
    FROM session_checkpoints c
    INNER JOIN session_retained_size z ON z.session_id = c.session_id
    WHERE c.id = old.checkpoint_id;
    UPDATE session_retained_size
    SET pane_state_row_bytes = pane_state_row_bytes - (
            8 + 8 + 8
            + COALESCE(length(CAST(old.cwd AS BLOB)), 0)
            + COALESCE(length(CAST(old.command AS BLOB)), 0)
            + COALESCE(length(CAST(old.env_json AS BLOB)), 0)
            + length(CAST(old.terminal_state_json AS BLOB))
            + COALESCE(length(CAST(old.agent_metadata_json AS BLOB)), 0)
            + CASE WHEN old.scrollback_checkpoint_seq IS NULL THEN 0 ELSE 8 END
            + CASE WHEN old.last_output_at IS NULL THEN 0 ELSE 8 END
        )
    WHERE session_id = (
        SELECT session_id FROM session_checkpoints WHERE id = old.checkpoint_id
    );
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_retained_size_ai
AFTER INSERT ON restore_attempt_lifecycle BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = new.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during lifecycle insert') END;
    SELECT CASE WHEN retained_bytes > 9223372036854775807 - (
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + CASE WHEN new.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(new.status AS BLOB))
        + 8
        + CASE WHEN new.resolved_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size overflow during lifecycle insert') END
    FROM session_retained_size WHERE session_id = new.session_id;
    UPDATE session_retained_size
    SET restore_lifecycle_row_bytes = restore_lifecycle_row_bytes
        + 8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + CASE WHEN new.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(new.status AS BLOB))
        + 8
        + CASE WHEN new.resolved_at IS NULL THEN 0 ELSE 8 END
    WHERE session_id = new.session_id;
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_retained_size_au
AFTER UPDATE ON restore_attempt_lifecycle BEGIN
    SELECT CASE WHEN new.intent_checkpoint_id != old.intent_checkpoint_id OR
                     new.session_id != old.session_id THEN
        RAISE(ABORT, 'restore-lifecycle retained-size identity is immutable')
    END;
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during lifecycle update') END;
    SELECT CASE WHEN restore_lifecycle_row_bytes < (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(old.status AS BLOB))
        + 8
        + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during lifecycle update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    SELECT CASE WHEN (
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + CASE WHEN new.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(new.status AS BLOB))
        + 8
        + CASE WHEN new.resolved_at IS NULL THEN 0 ELSE 8 END
    ) > (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(old.status AS BLOB))
        + 8
        + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
    ) AND retained_bytes > 9223372036854775807 - ((
        8
        + length(CAST(new.session_id AS BLOB))
        + 8
        + CASE WHEN new.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(new.status AS BLOB))
        + 8
        + CASE WHEN new.resolved_at IS NULL THEN 0 ELSE 8 END
    ) - (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(old.status AS BLOB))
        + 8
        + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
    )) THEN RAISE(ABORT, 'session retained-size overflow during lifecycle update') END
    FROM session_retained_size WHERE session_id = old.session_id;
    UPDATE session_retained_size
    SET restore_lifecycle_row_bytes = restore_lifecycle_row_bytes
        - (
            8
            + length(CAST(old.session_id AS BLOB))
            + 8
            + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
            + length(CAST(old.status AS BLOB))
            + 8
            + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
        )
        + (
            8
            + length(CAST(new.session_id AS BLOB))
            + 8
            + CASE WHEN new.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
            + length(CAST(new.status AS BLOB))
            + 8
            + CASE WHEN new.resolved_at IS NULL THEN 0 ELSE 8 END
        )
    WHERE session_id = old.session_id;
END;

CREATE TRIGGER IF NOT EXISTS restore_attempt_lifecycle_retained_size_ad
AFTER DELETE ON restore_attempt_lifecycle BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1 FROM mux_sessions WHERE session_id = old.session_id
    ) AND NOT EXISTS (
        SELECT 1 FROM session_retained_size WHERE session_id = old.session_id
    ) THEN RAISE(ABORT, 'missing session retained-size authority during lifecycle delete') END;
    SELECT CASE WHEN restore_lifecycle_row_bytes < (
        8
        + length(CAST(old.session_id AS BLOB))
        + 8
        + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
        + length(CAST(old.status AS BLOB))
        + 8
        + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
    ) THEN RAISE(ABORT, 'session retained-size underflow during lifecycle delete') END
    FROM session_retained_size WHERE session_id = old.session_id;
    UPDATE session_retained_size
    SET restore_lifecycle_row_bytes = restore_lifecycle_row_bytes - (
            8
            + length(CAST(old.session_id AS BLOB))
            + 8
            + CASE WHEN old.outcome_checkpoint_id IS NULL THEN 0 ELSE 8 END
            + length(CAST(old.status AS BLOB))
            + 8
            + CASE WHEN old.resolved_at IS NULL THEN 0 ELSE 8 END
        )
    WHERE session_id = old.session_id;
END;
-- FT_SESSION_RETAINED_SIZE_V40_END

-- Action history view (audit + undo + workflow step info)
CREATE VIEW IF NOT EXISTS action_history AS
SELECT a.*,
       u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by,
       w.workflow_id, w.step_name
FROM audit_actions a
LEFT JOIN action_undo u ON u.audit_action_id = a.id
LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;
"#;
