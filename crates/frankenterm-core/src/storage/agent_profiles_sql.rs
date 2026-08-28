//! StorageBackend CRUD primitives for the `agent_profiles` table
//! (br-ft-43lpu / ft-4yr9i.cont).
//!
//! Synchronous handlers callable from the storage writer thread
//! (or from tests via an in-memory backend). The async StorageHandle
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

use crate::agent_profiles::{AgentProfile, ProfileValidationError};
use crate::storage_backend_helpers::execute_typed;
use crate::storage_backend_row_helpers::CellRowReader;
use crate::storage_backend_trait::{BackendError, SqlCell, StorageBackend, ToSqlValue};
#[cfg(test)]
use crate::storage_backend_trait::{OpenConfig, RusqliteBackend};

/// Error type for the agent_profiles SQL primitives. Wraps
/// the storage backend error type, the substrate's validation
/// error, and JSON decode failures so the caller can distinguish
/// 'malformed row in DB' from 'caller passed bad input' from
/// 'SQL itself failed'.
#[derive(Debug)]
pub enum AgentProfileSqlError {
    /// Underlying storage backend call failed.
    Backend(BackendError),
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
            Self::Backend(e) => write!(f, "agent_profiles storage backend error: {e}"),
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
            Self::Backend(e) => Some(e),
            Self::Invalid(_) | Self::Decode { .. } => None,
        }
    }
}

impl From<BackendError> for AgentProfileSqlError {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
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
/// A duplicate name returns `AgentProfileSqlError::Backend`
/// wrapping the backend UNIQUE constraint error — caller can
/// match on it for 'replace vs insert' logic.
pub fn insert_agent_profile(
    backend: &dyn StorageBackend,
    profile: &AgentProfile,
) -> Result<String, AgentProfileSqlError> {
    profile.validate()?;
    let tags_json = serde_json::to_string(&profile.tags).expect("tags serialize");
    let env_json = serde_json::to_string(&profile.env).expect("env serialize");
    let metadata_json = serde_json::to_string(&profile.metadata).expect("metadata serialize");
    execute_typed(
        backend,
        "INSERT INTO agent_profiles
         (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        &[
            ToSqlValue::Text(profile.name.as_str()),
            ToSqlValue::Text(profile.role.as_str()),
            ToSqlValue::OwnedText(tags_json),
            ToSqlValue::Text(profile.shell.as_str()),
            ToSqlValue::optional_text(profile.command.as_deref()),
            ToSqlValue::OwnedText(env_json),
            ToSqlValue::OwnedText(metadata_json),
            ToSqlValue::Integer(profile.created_at_ms),
            ToSqlValue::Integer(profile.updated_at_ms),
        ],
    )?;
    Ok(profile.name.clone())
}

/// Get a profile by name. Returns `None` if no row matches.
pub fn get_agent_profile(
    backend: &dyn StorageBackend,
    name: &str,
) -> Result<Option<AgentProfile>, AgentProfileSqlError> {
    let row = backend.query_row_cells(
        "SELECT name, role, tags, shell, command, env, metadata,
                created_at_ms, updated_at_ms
         FROM agent_profiles
         WHERE name = ?1",
        &[ToSqlValue::Text(name)],
    )?;
    row.as_deref().map(agent_profile_from_cells).transpose()
}

/// List profiles. When `role_filter` is `Some`, restrict to
/// profiles with the matching `role` column (uses the
/// `agent_profiles_role_idx` from the migration). When `None`,
/// returns every row, ordered by `name` ASC for stable output.
pub fn list_agent_profiles(
    backend: &dyn StorageBackend,
    role_filter: Option<&str>,
) -> Result<Vec<AgentProfile>, AgentProfileSqlError> {
    let rows = match role_filter {
        Some(role) => backend.query_map_cells(
            "SELECT name, role, tags, shell, command, env, metadata,
                    created_at_ms, updated_at_ms
             FROM agent_profiles
             WHERE role = ?1
             ORDER BY name ASC",
            &[ToSqlValue::Text(role)],
        ),
        None => backend.query_map_cells(
            "SELECT name, role, tags, shell, command, env, metadata,
                    created_at_ms, updated_at_ms
             FROM agent_profiles
             ORDER BY name ASC",
            &[],
        ),
    }?;
    rows.iter()
        .map(|row| agent_profile_from_cells(row))
        .collect()
}

/// Delete a profile by name. Returns `true` if a row was
/// removed, `false` if no row matched (operator-friendly so
/// 'delete --name foo' can distinguish 'foo never existed' from
/// 'foo was deleted').
pub fn delete_agent_profile(
    backend: &dyn StorageBackend,
    name: &str,
) -> Result<bool, AgentProfileSqlError> {
    let row = backend.query_row_typed(
        "DELETE FROM agent_profiles WHERE name = ?1 RETURNING 1",
        &[ToSqlValue::Text(name)],
    )?;
    Ok(row.is_some())
}

/// Row deserializer used by the SELECT helpers. Returns a
/// backend-level error when row shape/type extraction fails, or a
/// decode error when a JSON column payload is malformed.
fn agent_profile_from_cells(row: &[SqlCell]) -> Result<AgentProfile, AgentProfileSqlError> {
    let reader = CellRowReader::new(row);
    let tags_json = reader.string(2)?;
    let env_json = reader.string(5)?;
    let metadata_json = reader.string(6)?;

    let tags: Vec<String> = match serde_json::from_str(&tags_json) {
        Ok(v) => v,
        Err(e) => {
            return Err(AgentProfileSqlError::Decode {
                column: "tags",
                msg: e.to_string(),
            });
        }
    };
    let env: HashMap<String, String> = match serde_json::from_str(&env_json) {
        Ok(v) => v,
        Err(e) => {
            return Err(AgentProfileSqlError::Decode {
                column: "env",
                msg: e.to_string(),
            });
        }
    };
    let metadata: HashMap<String, String> = match serde_json::from_str(&metadata_json) {
        Ok(v) => v,
        Err(e) => {
            return Err(AgentProfileSqlError::Decode {
                column: "metadata",
                msg: e.to_string(),
            });
        }
    };

    Ok(AgentProfile {
        name: reader.string(0)?,
        role: reader.string(1)?,
        tags,
        shell: reader.string(3)?,
        command: reader.optional_string(4)?,
        env,
        metadata,
        created_at_ms: reader.i64(7)?,
        updated_at_ms: reader.i64(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> RusqliteBackend {
        let backend = RusqliteBackend::open(
            ":memory:",
            &OpenConfig {
                wal_mode: false,
                ..OpenConfig::default()
            },
        )
        .expect("open in-memory");
        backend
            .execute_batch(
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
        backend
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
        let conn = fresh_db();
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
        let conn = fresh_db();
        let out = get_agent_profile(&conn, "nobody").unwrap();
        assert!(out.is_none());
    }

    /// br-ft-43lpu: insert validates via the substrate's
    /// AgentProfile::validate before touching SQLite. Empty name
    /// is the simplest invariant violation.
    #[test]
    fn insert_validates_via_substrate_before_sqlite() {
        let conn = fresh_db();
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
    /// (PRIMARY KEY). Wrapped in Backend variant so a 'replace
    /// vs insert' caller can match on it.
    #[test]
    fn insert_duplicate_name_is_backend_error() {
        let conn = fresh_db();
        let p = synth_profile("alice", "ops");
        insert_agent_profile(&conn, &p).expect("first insert");
        let err = insert_agent_profile(&conn, &p).unwrap_err();
        match err {
            AgentProfileSqlError::Backend(_) => {}
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    /// br-ft-43lpu: list with no filter returns every profile
    /// ordered by name ASC.
    #[test]
    fn list_all_profiles_ordered_by_name() {
        let conn = fresh_db();
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
        let conn = fresh_db();
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
        let conn = fresh_db();
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
        let conn = fresh_db();
        // Insert directly via raw SQL with deliberately
        // malformed tags.
        execute_typed(
            &conn,
            "INSERT INTO agent_profiles
             (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            &[
                ToSqlValue::Text("alice"),
                ToSqlValue::Text("ops"),
                ToSqlValue::Text("not_valid_json["),
                ToSqlValue::Text("/bin/zsh"),
                ToSqlValue::Null,
                ToSqlValue::Text("{}"),
                ToSqlValue::Text("{}"),
                ToSqlValue::Integer(1_700_000_000_000_i64),
                ToSqlValue::Integer(1_700_000_000_000_i64),
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
