//! SQLite CRUD primitives for the `agent_profiles` table
//! (br-ft-43lpu / ft-4yr9i.cont).
//!
//! Synchronous handlers callable from the storage writer thread
//! (or from tests via in-memory SQLite). The async StorageHandle
//! wrappers + WriteCommand variants are a separate slice — this
//! module ships the actual SQL so a future commit can plug them
//! into the writer-loop dispatch table without re-litigating
//! schema details.
//!
//! Schema lives in storage/migrations.rs at version 25 (shipped
//! at 10ee0b5fd via br-ft-4yr9i). The substrate's
//! `AgentProfile::validate` is run before every insert so the
//! integrity rules in agent_profiles.rs stay in lockstep with
//! what hits SQLite.
//!
//! JSON-encoded TEXT columns: `tags` (Vec<String>), `env`
//! (HashMap<String, String>), `metadata` (HashMap<String,
//! String>). serde_json round-trip; an unparseable value in the
//! DB is mapped to `AgentProfileSqlError::Decode`.
//!
//! Cross-references:
//! - parent: ft-4yr9i (migration step shipped at 10ee0b5fd)
//! - substrate: crates/frankenterm-core/src/agent_profiles.rs
//! - sibling: ft-df3cz (substrate slice)

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::agent_profiles::{AgentProfile, ProfileValidationError};

/// Error type for the agent_profiles SQL primitives. Wraps
/// rusqlite's error type, the substrate's validation error, and
/// JSON decode failures so the caller can distinguish 'malformed
/// row in DB' from 'caller passed bad input' from 'SQL itself
/// failed'.
#[derive(Debug)]
pub enum AgentProfileSqlError {
    /// Underlying SQLite call failed.
    Sqlite(rusqlite::Error),
    /// Substrate's `AgentProfile::validate` rejected the input
    /// before the insert was attempted.
    Invalid(ProfileValidationError),
    /// A JSON-encoded TEXT column (tags / env / metadata) failed
    /// to decode. Carries the column name + serde_json error
    /// message for operator diagnosis. Always indicates DB
    /// corruption (the insert side runs serde_json::to_string
    /// which is infallible for our owned types).
    Decode { column: &'static str, msg: String },
}

impl core::fmt::Display for AgentProfileSqlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "agent_profiles SQLite error: {e}"),
            Self::Invalid(v) => {
                write!(f, "agent_profiles validation rejected input: {v:?}")
            }
            Self::Decode { column, msg } => write!(
                f,
                "agent_profiles column {column}: JSON decode failed: {msg}",
            ),
        }
    }
}

impl std::error::Error for AgentProfileSqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Invalid(_) | Self::Decode { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for AgentProfileSqlError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<ProfileValidationError> for AgentProfileSqlError {
    fn from(e: ProfileValidationError) -> Self {
        Self::Invalid(e)
    }
}

/// Insert a profile, validating first via the substrate's
/// `AgentProfile::validate`. Returns the row's `name` (which is
/// the PRIMARY KEY) on success.
///
/// A duplicate name returns `AgentProfileSqlError::Sqlite`
/// wrapping the SQLite UNIQUE constraint error — caller can
/// match on it for 'replace vs insert' logic.
pub fn insert_agent_profile(
    conn: &Connection,
    profile: &AgentProfile,
) -> Result<String, AgentProfileSqlError> {
    profile.validate()?;
    let tags_json = serde_json::to_string(&profile.tags).expect("tags serialize");
    let env_json = serde_json::to_string(&profile.env).expect("env serialize");
    let metadata_json = serde_json::to_string(&profile.metadata).expect("metadata serialize");
    conn.execute(
        "INSERT INTO agent_profiles
         (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            profile.name,
            profile.role,
            tags_json,
            profile.shell,
            profile.command,
            env_json,
            metadata_json,
            profile.created_at_ms,
            profile.updated_at_ms,
        ],
    )?;
    Ok(profile.name.clone())
}

/// Get a profile by name. Returns `None` if no row matches.
pub fn get_agent_profile(
    conn: &Connection,
    name: &str,
) -> Result<Option<AgentProfile>, AgentProfileSqlError> {
    conn.query_row(
        "SELECT name, role, tags, shell, command, env, metadata,
                created_at_ms, updated_at_ms
         FROM agent_profiles
         WHERE name = ?1",
        params![name],
        row_from_sql,
    )
    .optional()
    .map_err(AgentProfileSqlError::from)?
    .transpose()
}

/// List profiles. When `role_filter` is `Some`, restrict to
/// profiles with the matching `role` column (uses the
/// `agent_profiles_role_idx` from the migration). When `None`,
/// returns every row, ordered by `name` ASC for stable output.
pub fn list_agent_profiles(
    conn: &Connection,
    role_filter: Option<&str>,
) -> Result<Vec<AgentProfile>, AgentProfileSqlError> {
    // Two paths so the rusqlite::ToSql borrow stays inside the
    // function body. Keeping the SELECT column list literal-
    // identical between branches lets row_from_sql work for
    // both unchanged.
    let rows: Vec<rusqlite::Result<Result<AgentProfile, AgentProfileSqlError>>> = match role_filter
    {
        Some(r) => {
            let mut stmt = conn.prepare(
                "SELECT name, role, tags, shell, command, env, metadata,
                            created_at_ms, updated_at_ms
                     FROM agent_profiles
                     WHERE role = ?1
                     ORDER BY name ASC",
            )?;
            stmt.query_map(params![r], row_from_sql)?.collect()
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT name, role, tags, shell, command, env, metadata,
                            created_at_ms, updated_at_ms
                     FROM agent_profiles
                     ORDER BY name ASC",
            )?;
            stmt.query_map([], row_from_sql)?.collect()
        }
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row?);
    }
    out.into_iter().collect()
}

/// Delete a profile by name. Returns `true` if a row was
/// removed, `false` if no row matched (operator-friendly so
/// 'delete --name foo' can distinguish 'foo never existed' from
/// 'foo was deleted').
pub fn delete_agent_profile(conn: &Connection, name: &str) -> Result<bool, AgentProfileSqlError> {
    let n = conn.execute("DELETE FROM agent_profiles WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

/// Row deserializer used by the SELECT helpers. Returns a
/// `rusqlite::Result<Result<AgentProfile, AgentProfileSqlError>>`
/// shape so JSON-decode failures propagate via the inner error
/// type (the SQLite layer is fine — the row exists; the column
/// payload is malformed).
fn row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<AgentProfile, AgentProfileSqlError>> {
    let name: String = row.get(0)?;
    let role: String = row.get(1)?;
    let tags_json: String = row.get(2)?;
    let shell: String = row.get(3)?;
    let command: Option<String> = row.get(4)?;
    let env_json: String = row.get(5)?;
    let metadata_json: String = row.get(6)?;
    let created_at_ms: i64 = row.get(7)?;
    let updated_at_ms: i64 = row.get(8)?;

    let tags: Vec<String> = match serde_json::from_str(&tags_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Err(AgentProfileSqlError::Decode {
                column: "tags",
                msg: e.to_string(),
            }));
        }
    };
    let env: HashMap<String, String> = match serde_json::from_str(&env_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Err(AgentProfileSqlError::Decode {
                column: "env",
                msg: e.to_string(),
            }));
        }
    };
    let metadata: HashMap<String, String> = match serde_json::from_str(&metadata_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Err(AgentProfileSqlError::Decode {
                column: "metadata",
                msg: e.to_string(),
            }));
        }
    };

    Ok(Ok(AgentProfile {
        name,
        role,
        tags,
        shell,
        command,
        env,
        metadata,
        created_at_ms,
        updated_at_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_profiles (
                name           TEXT PRIMARY KEY NOT NULL,
                role           TEXT NOT NULL DEFAULT '',
                tags           TEXT NOT NULL DEFAULT '[]',
                shell          TEXT NOT NULL DEFAULT '',
                command        TEXT,
                env            TEXT NOT NULL DEFAULT '{}',
                metadata       TEXT NOT NULL DEFAULT '{}',
                created_at_ms  INTEGER NOT NULL,
                updated_at_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS agent_profiles_role_idx
                ON agent_profiles(role);",
        )
        .expect("schema");
        conn
    }

    fn synth_profile(name: &str, role: &str) -> AgentProfile {
        let mut env = HashMap::new();
        env.insert("EDITOR".to_string(), "vim".to_string());
        let mut metadata = HashMap::new();
        metadata.insert("team".to_string(), "platform".to_string());
        AgentProfile {
            name: name.to_string(),
            role: role.to_string(),
            tags: vec!["work".to_string(), "rust".to_string()],
            shell: "/bin/zsh".to_string(),
            command: Some("nvim".to_string()),
            env,
            metadata,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_000,
        }
    }

    /// br-ft-43lpu: insert + get round-trips every field
    /// byte-for-byte through the JSON-encoded TEXT columns.
    #[test]
    fn insert_and_get_roundtrip_preserves_every_field() {
        let conn = fresh_conn();
        let p = synth_profile("alice", "ops");
        let returned = insert_agent_profile(&conn, &p).expect("insert");
        assert_eq!(returned, "alice");

        let fetched = get_agent_profile(&conn, "alice")
            .expect("get")
            .expect("row exists");
        assert_eq!(p, fetched);
    }

    /// br-ft-43lpu: get on a missing name returns Ok(None).
    /// Operator-facing 'profile not found' relies on this.
    #[test]
    fn get_missing_name_returns_none() {
        let conn = fresh_conn();
        let out = get_agent_profile(&conn, "nobody").unwrap();
        assert!(out.is_none());
    }

    /// br-ft-43lpu: insert validates via the substrate's
    /// AgentProfile::validate before touching SQLite. Empty name
    /// is the simplest invariant violation.
    #[test]
    fn insert_validates_via_substrate_before_sqlite() {
        let conn = fresh_conn();
        let mut bad = synth_profile("", "ops"); // empty name
        bad.name = String::new();
        let err = insert_agent_profile(&conn, &bad).unwrap_err();
        match err {
            AgentProfileSqlError::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
        // Confirm no row was inserted.
        let listed = list_agent_profiles(&conn, None).unwrap();
        assert!(listed.is_empty());
    }

    /// br-ft-43lpu: duplicate name → SQLite UNIQUE constraint
    /// (PRIMARY KEY). Wrapped in Sqlite variant so a 'replace
    /// vs insert' caller can match on it.
    #[test]
    fn insert_duplicate_name_is_sqlite_error() {
        let conn = fresh_conn();
        let p = synth_profile("alice", "ops");
        insert_agent_profile(&conn, &p).expect("first insert");
        let err = insert_agent_profile(&conn, &p).unwrap_err();
        match err {
            AgentProfileSqlError::Sqlite(_) => {}
            other => panic!("expected Sqlite, got {other:?}"),
        }
    }

    /// br-ft-43lpu: list with no filter returns every profile
    /// ordered by name ASC.
    #[test]
    fn list_all_profiles_ordered_by_name() {
        let conn = fresh_conn();
        for name in ["zeta", "alice", "marcus"] {
            insert_agent_profile(&conn, &synth_profile(name, "ops")).unwrap();
        }
        let listed = list_agent_profiles(&conn, None).unwrap();
        let names: Vec<String> = listed.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["alice", "marcus", "zeta"]);
    }

    /// br-ft-43lpu: list with a role filter returns only
    /// matching rows. Uses the role index from the migration.
    #[test]
    fn list_with_role_filter_restricts_results() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth_profile("alice", "ops")).unwrap();
        insert_agent_profile(&conn, &synth_profile("bob", "dev")).unwrap();
        insert_agent_profile(&conn, &synth_profile("carol", "ops")).unwrap();

        let ops = list_agent_profiles(&conn, Some("ops")).unwrap();
        assert_eq!(ops.len(), 2);
        let names: Vec<String> = ops.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["alice", "carol"]);

        let dev = list_agent_profiles(&conn, Some("dev")).unwrap();
        assert_eq!(dev.len(), 1);
        assert_eq!(dev[0].name, "bob");

        let missing = list_agent_profiles(&conn, Some("manager")).unwrap();
        assert!(missing.is_empty());
    }

    /// br-ft-43lpu: delete returns true on hit, false on miss.
    #[test]
    fn delete_returns_true_on_hit_false_on_miss() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth_profile("alice", "ops")).unwrap();
        assert!(delete_agent_profile(&conn, "alice").unwrap());
        // Now it's gone.
        assert!(!delete_agent_profile(&conn, "alice").unwrap());
        // And missing names also return false.
        assert!(!delete_agent_profile(&conn, "nobody").unwrap());
    }

    /// br-ft-43lpu: corrupted JSON in the tags column surfaces
    /// as AgentProfileSqlError::Decode rather than a panic. The
    /// integration's read path can fall back to the bookkeeping
    /// or surface 'profile X has malformed tags' to the
    /// operator.
    #[test]
    fn malformed_tags_json_returns_decode_error() {
        let conn = fresh_conn();
        // Insert directly via raw SQL with deliberately
        // malformed tags.
        conn.execute(
            "INSERT INTO agent_profiles
             (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                "alice",
                "ops",
                "not_valid_json[",
                "/bin/zsh",
                Option::<String>::None,
                "{}",
                "{}",
                1_700_000_000_000_i64,
                1_700_000_000_000_i64,
            ],
        )
        .unwrap();
        let err = get_agent_profile(&conn, "alice").unwrap_err();
        match err {
            AgentProfileSqlError::Decode { column, .. } => {
                assert_eq!(column, "tags");
            }
            other => panic!("expected Decode, got {other:?}"),
        }
    }
}
