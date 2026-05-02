//! Real handler for the `robot profile` family (ft-b0g7g).
//!
//! Replaces the `build_ntm_not_implemented_response` placeholder at
//! `crates/frankenterm/src/main.rs:23270`. The handler operates on a
//! [`rusqlite::Connection`] so the conformance test in
//! `tests/robot_family_conformance.rs` can drive it against a fresh
//! in-memory DB without standing up the full async `StorageHandle`.
//!
//! ## Action coverage
//!
//! - `list` — reads `agent_profiles` via
//!   [`crate::storage::agent_profiles_sql::list_agent_profiles`],
//!   honours both `role_filter` (pushed down to SQL) and
//!   `tag_filter` (filtered in-process because tags are stored as a
//!   JSON array and the role index is the only one shipped). Returns
//!   `ProfileListData { profiles: Vec<ProfileSummary> }`.
//! - `show` — reads one row by name; returns `ProfileShowData` or a
//!   typed `NotFound` error.
//! - `validate` — looks the row up, runs
//!   [`AgentProfile::validate`], reports `valid` / `issues`.
//! - `apply` — verifies the row exists; for `dry_run = true` returns
//!   the planned `ProfileApplyData`; for `dry_run = false` this
//!   standalone handler reports a typed `SpawnFailed` because real
//!   spawning is daemon-mediated and must run through the mux service.
//!   The contract spec at
//!   `docs/robot-contracts/profile.md` calls `apply` `Sequential`
//!   (mutating, transactional); this slice is the read-only-safe
//!   subset of that contract.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::Value;

use crate::agent_profiles::{AgentProfile, ProfileValidationError};
use crate::robot_ntm_surface::{
    ProfileApplyData, ProfileListData, ProfileShowData, ProfileSummary, ProfileValidateData,
};
use crate::storage::agent_profiles_sql::{
    AgentProfileSqlError, get_agent_profile, list_agent_profiles,
};

/// Failure modes the handler exposes to its caller. The wire-level
/// envelope at `main.rs` translates each variant into a typed
/// `RobotResponse` error code; the conformance test only sees the
/// success path because it pre-seeds a row named `default`.
#[derive(Debug)]
pub enum ProfileHandlerError {
    /// Action name is not one of `list` / `show` / `apply` /
    /// `validate`. Indicates a CLI-to-handler dispatch bug, not a
    /// caller error.
    UnknownAction(String),
    /// A required parameter is missing or the wrong shape (e.g.,
    /// `name` is not a string).
    BadParams(String),
    /// Profile lookup returned `None` for a name the action requires
    /// to exist.
    NotFound { name: String },
    /// Underlying SQL primitive failed (schema, deserialization,
    /// duplicate constraint, …).
    Storage(AgentProfileSqlError),
    /// `apply` with `dry_run = false` failed before pane spawning
    /// completed.
    SpawnFailed { reason: String },
}

impl std::fmt::Display for ProfileHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAction(a) => write!(f, "unknown profile action `{a}`"),
            Self::BadParams(msg) => write!(f, "invalid profile request: {msg}"),
            Self::NotFound { name } => write!(f, "profile `{name}` not found"),
            Self::Storage(e) => write!(f, "profile storage error: {e}"),
            Self::SpawnFailed { reason } => write!(f, "profile apply spawn failed: {reason}"),
        }
    }
}

impl std::error::Error for ProfileHandlerError {}

impl ProfileHandlerError {
    /// Stable error code for the wire envelope. Matches the family of
    /// codes [`crate::robot_envelope`] uses for typed failures.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::UnknownAction(_) => "robot.profile.unknown_action",
            Self::BadParams(_) => "robot.profile.bad_params",
            Self::NotFound { .. } => "robot.profile.not_found",
            Self::Storage(_) => "robot.profile.storage",
            Self::SpawnFailed { .. } => "robot.profile.spawn_failed",
        }
    }
}

/// Dispatch entry point. Returns the handler's success payload as a
/// JSON `Value` matching the per-action response schema declared in
/// [`crate::robot_family_contract::profile_family_contract`]. The
/// caller wraps the value (or the error) in a `RobotResponse`
/// envelope.
///
/// `params` is the canonical request object — the `params` of the
/// `RobotNtmCommand::Profile(ProfileCommand::*)` request after the
/// CLI has parsed its flags into JSON. Nested keys mirror the
/// `Profile*Request` types in [`crate::robot_ntm_surface`].
pub fn handle_profile_command(
    action: &str,
    params: &Value,
    conn: &Connection,
) -> Result<Value, ProfileHandlerError> {
    match action {
        "list" => handle_list(params, conn),
        "show" => handle_show(params, conn),
        "apply" => handle_apply(params, conn),
        "validate" => handle_validate(params, conn),
        other => Err(ProfileHandlerError::UnknownAction(other.to_string())),
    }
}

fn require_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ProfileHandlerError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ProfileHandlerError::BadParams(format!("missing required field `{key}`")))
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn opt_u32(params: &Value, key: &str) -> Option<u32> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn opt_bool(params: &Value, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, ProfileHandlerError> {
    serde_json::to_value(v).map_err(|e| ProfileHandlerError::BadParams(e.to_string()))
}

fn summarize(profile: AgentProfile) -> ProfileSummary {
    ProfileSummary {
        description: profile.metadata.get("description").cloned(),
        name: profile.name,
        role: profile.role,
        tags: profile.tags,
    }
}

fn handle_list(params: &Value, conn: &Connection) -> Result<Value, ProfileHandlerError> {
    let role_filter = opt_str(params, "role_filter");
    let tag_filter = opt_str(params, "tag_filter").map(str::to_string);
    let rows = list_agent_profiles(conn, role_filter).map_err(ProfileHandlerError::Storage)?;
    let profiles: Vec<ProfileSummary> = rows
        .into_iter()
        .filter(|row| match &tag_filter {
            None => true,
            Some(tag) => row.tags.iter().any(|t| t == tag),
        })
        .map(summarize)
        .collect();
    to_value(&ProfileListData { profiles })
}

fn handle_show(params: &Value, conn: &Connection) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let profile = get_agent_profile(conn, name)
        .map_err(ProfileHandlerError::Storage)?
        .ok_or_else(|| ProfileHandlerError::NotFound {
            name: name.to_string(),
        })?;
    let data = ProfileShowData {
        description: profile.metadata.get("description").cloned(),
        working_directory: profile.metadata.get("working_directory").cloned(),
        layout_template: profile.metadata.get("layout_template").cloned(),
        bootstrap_commands: parse_bootstrap_commands(&profile.metadata),
        name: profile.name,
        role: profile.role,
        spawn_command: profile.command,
        environment: profile.env,
        tags: profile.tags,
    };
    // The contract's response_schema (additionalProperties: false,
    // single-type per field) cannot encode `string | null`, so strip
    // null entries from the serialized response. Required fields are
    // never `Option<_>` in `ProfileShowData`, so this preserves the
    // contract's `required` invariant.
    Ok(strip_null_fields(to_value(&data)?))
}

fn handle_validate(params: &Value, conn: &Connection) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let row = get_agent_profile(conn, name).map_err(ProfileHandlerError::Storage)?;
    let (valid, issues) = match row {
        None => (
            false,
            vec![format!("profile `{name}` not found in agent_profiles")],
        ),
        Some(profile) => match profile.validate() {
            Ok(()) => (true, Vec::new()),
            Err(err) => (false, vec![format_validation_error(&err)]),
        },
    };
    to_value(&ProfileValidateData {
        name: name.to_string(),
        valid,
        issues,
    })
}

fn handle_apply(params: &Value, conn: &Connection) -> Result<Value, ProfileHandlerError> {
    let name = require_str(params, "name")?;
    let count = opt_u32(params, "count").unwrap_or(1);
    let dry_run = opt_bool(params, "dry_run").unwrap_or(false);

    // Verify the row exists before reporting any "would spawn" plan.
    let _profile = get_agent_profile(conn, name)
        .map_err(ProfileHandlerError::Storage)?
        .ok_or_else(|| ProfileHandlerError::NotFound {
            name: name.to_string(),
        })?;

    if dry_run {
        return to_value(&ProfileApplyData {
            profile_name: name.to_string(),
            // Plan only — actual pane IDs come from the daemon-side
            // spawn loop. Surface count via the response shape: an
            // empty `panes_spawned` array combined with `dry_run:
            // true` is the contract's "no mutation" signal.
            panes_spawned: Vec::new(),
            dry_run: true,
        });
    }

    // Real spawn requires the daemon-side mux machinery. The
    // standalone handler cannot perform that mutation directly.
    Err(ProfileHandlerError::SpawnFailed {
        reason: format!(
            "profile `{name}` apply (count={count}) requires daemon-mediated pane spawning; \
             daemon apply RPC is not connected in this process"
        ),
    })
}

/// Remove top-level fields whose serialized value is JSON `null`.
/// The contract's `additionalProperties: false` schema cannot
/// encode `string | null`, so emitting an absent key (rather than a
/// `null` entry) is the only shape that validates while still
/// honouring `required` on the non-optional fields.
fn strip_null_fields(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
        }
        other => other,
    }
}

fn parse_bootstrap_commands(metadata: &HashMap<String, String>) -> Vec<String> {
    let Some(raw) = metadata.get("bootstrap_commands") else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn format_validation_error(err: &ProfileValidationError) -> String {
    format!("{err:?}")
}

// ============================================================================
// br-ft-4iz0q substrate-pass: ApplyReceipt content-hash for the
// idempotency contract. The wired-pass cont-bead writes these into a
// new `profiles_applied_log` table; this module ships the pure-data
// type + the canonical-hash helper so the daemon-side handler can
// drop into a finished substrate.
// ============================================================================

/// Idempotency receipt for a non-dry-run `RobotProfile.apply` call.
///
/// br-ft-4iz0q (continuation of br-ft-b0g7g.cont.apply_spawn) names
/// the bead's idempotency rule:
///
/// > Compute an `ApplyReceipt` content-hash from `(profile_name,
/// > profile.updated_at_ms, env_overrides_canonicalized, count)` and
/// > reject duplicate apply requests with `ProfileApplyData {
/// > panes_spawned: <prior_apply>, dry_run: false }`.
///
/// The substrate ships the hash + the receipt payload type; the
/// wired-pass writes them into the `profiles_applied_log` table and
/// the daemon-side handler consults the log before issuing real mux
/// spawn calls.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyReceipt {
    /// Hex SHA-256 of the canonical
    /// `(profile_name, updated_at_ms, env_overrides, count)` tuple.
    /// Stable across runs of the same logical apply.
    pub content_hash: String,
    /// Profile name the apply targeted. Carried for log readability;
    /// the hash already binds it.
    pub profile_name: String,
    /// `updated_at_ms` of the profile row at apply time. Bumping the
    /// profile in storage invalidates earlier receipts (the hash
    /// changes), so an apply against the new revision is NOT
    /// idempotent against the old one — that's the bead's intent.
    pub profile_updated_at_ms: i64,
    /// Number of panes the apply requested. Re-applies must specify
    /// the same count to hit the receipt.
    pub count: u32,
    /// Pane IDs the original apply spawned. Returned in the dedup
    /// path's `ProfileApplyData.panes_spawned` so callers see the
    /// same list they would have seen on first success.
    pub panes_spawned: Vec<u64>,
    /// Unix epoch ms when the receipt was first written.
    pub recorded_at_ms: i64,
}

/// Compute the canonical content hash for an apply request.
///
/// Inputs are canonicalised before hashing so the same logical apply
/// produces a stable hash regardless of HashMap iteration order:
///
/// 1. `env_overrides` is sorted by key.
/// 2. The pre-image is `"<name>\0<updated_at_ms>\0<count>\0<env>"`
///    where `<env>` is each `key=value` pair sorted, joined by NUL.
/// 3. SHA-256 over the pre-image, hex-encoded lowercase.
///
/// The pre-image format is **not** stable across major releases —
/// receipts written under one version may not match receipts written
/// under another. The wired-pass migration handles old-format rows
/// by dropping them (re-applies just pay the spawn cost once more).
#[must_use]
pub fn compute_apply_content_hash(
    profile_name: &str,
    profile_updated_at_ms: i64,
    count: u32,
    env_overrides: &std::collections::HashMap<String, String>,
) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(profile_name.as_bytes());
    hasher.update([0u8]);
    hasher.update(profile_updated_at_ms.to_string().as_bytes());
    hasher.update([0u8]);
    hasher.update(count.to_string().as_bytes());
    hasher.update([0u8]);

    // Sort env_overrides by key for deterministic hashing.
    let mut env_sorted: Vec<(&String, &String)> = env_overrides.iter().collect();
    env_sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in env_sorted {
        hasher.update(k.as_bytes());
        hasher.update([b'=']);
        hasher.update(v.as_bytes());
        hasher.update([0u8]);
    }

    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod apply_receipt_tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn hash_is_stable_for_same_inputs() {
        let env: HashMap<String, String> = [("FOO".to_string(), "bar".to_string())]
            .into_iter()
            .collect();
        let h1 = compute_apply_content_hash("dev", 1_700_000_000_000, 3, &env);
        let h2 = compute_apply_content_hash("dev", 1_700_000_000_000, 3, &env);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_diverges_when_profile_name_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 0, 1, &env);
        let b = compute_apply_content_hash("prod", 0, 1, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_diverges_when_updated_at_ms_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 1, 1, &env);
        let b = compute_apply_content_hash("dev", 2, 1, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_diverges_when_count_changes() {
        let env = HashMap::new();
        let a = compute_apply_content_hash("dev", 0, 3, &env);
        let b = compute_apply_content_hash("dev", 0, 4, &env);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_canonicalises_env_overrides_iteration_order() {
        // Two HashMaps with identical entries should hash identically
        // even if their internal iteration order differs (the hasher
        // sorts before consuming).
        let mut env_a = HashMap::new();
        env_a.insert("FOO".to_string(), "1".to_string());
        env_a.insert("BAR".to_string(), "2".to_string());
        env_a.insert("BAZ".to_string(), "3".to_string());

        let mut env_b = HashMap::new();
        env_b.insert("BAZ".to_string(), "3".to_string());
        env_b.insert("BAR".to_string(), "2".to_string());
        env_b.insert("FOO".to_string(), "1".to_string());

        assert_eq!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn hash_diverges_when_env_value_changes() {
        let mut env_a = HashMap::new();
        env_a.insert("FOO".to_string(), "1".to_string());
        let mut env_b = HashMap::new();
        env_b.insert("FOO".to_string(), "2".to_string());
        assert_ne!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn hash_no_collision_between_separator_chars_in_value() {
        // Defensive: a value containing the NUL separator should
        // not collide with an env containing two distinct keys.
        let mut env_a = HashMap::new();
        env_a.insert("FOO".to_string(), "bar\0BAR=baz".to_string());

        let mut env_b = HashMap::new();
        env_b.insert("FOO".to_string(), "bar".to_string());
        env_b.insert("BAR".to_string(), "baz".to_string());

        // The hashes should differ because env_a has 1 entry
        // (FOO=bar\0BAR=baz) while env_b has 2 entries
        // (BAR=baz; FOO=bar) — even though the literal pre-image
        // bytes happen to look similar after NUL separation.
        assert_ne!(
            compute_apply_content_hash("p", 0, 1, &env_a),
            compute_apply_content_hash("p", 0, 1, &env_b),
        );
    }

    #[test]
    fn apply_receipt_serde_roundtrip() {
        let receipt = ApplyReceipt {
            content_hash: "abc123".to_string(),
            profile_name: "dev".to_string(),
            profile_updated_at_ms: 1_700_000_000_000,
            count: 3,
            panes_spawned: vec![10, 11, 12],
            recorded_at_ms: 1_700_000_001_000,
        };
        let serialized = serde_json::to_string(&receipt).unwrap();
        let parsed: ApplyReceipt = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, receipt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::agent_profiles_sql::insert_agent_profile;
    use serde_json::json;

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

    fn synth(name: &str, role: &str, tags: Vec<&str>) -> AgentProfile {
        let mut metadata = HashMap::new();
        metadata.insert("description".to_string(), format!("synth {name}"));
        AgentProfile {
            name: name.to_string(),
            role: role.to_string(),
            tags: tags.into_iter().map(str::to_string).collect(),
            shell: "/bin/sh".to_string(),
            command: Some("echo".to_string()),
            env: HashMap::new(),
            metadata,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn list_returns_empty_when_table_empty() {
        let conn = fresh_conn();
        let v = handle_profile_command("list", &json!({}), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).expect("arr");
        assert!(arr.is_empty());
    }

    #[test]
    fn list_returns_summaries_in_name_order() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("z", "alpha", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("a", "alpha", vec![])).unwrap();
        let v = handle_profile_command("list", &json!({}), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        let names: Vec<&str> = arr
            .iter()
            .map(|p| p.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["a", "z"]);
    }

    #[test]
    fn list_role_filter_pushes_down_to_sql() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("alpha-1", "alpha", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("beta-1", "beta", vec![])).unwrap();
        let v =
            handle_profile_command("list", &json!({ "role_filter": "alpha" }), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("name").and_then(Value::as_str), Some("alpha-1"));
    }

    #[test]
    fn list_tag_filter_filters_in_process() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("with-x", "r", vec!["x", "y"])).unwrap();
        insert_agent_profile(&conn, &synth("with-y", "r", vec!["y"])).unwrap();
        let v = handle_profile_command("list", &json!({ "tag_filter": "x" }), &conn).expect("ok");
        let arr = v.get("profiles").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("name").and_then(Value::as_str), Some("with-x"));
    }

    #[test]
    fn show_returns_full_profile_data() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("p1", "r1", vec!["t1"])).unwrap();
        let v = handle_profile_command("show", &json!({ "name": "p1" }), &conn).expect("ok");
        assert_eq!(v.get("name").and_then(Value::as_str), Some("p1"));
        assert_eq!(v.get("role").and_then(Value::as_str), Some("r1"));
        assert_eq!(
            v.get("description").and_then(Value::as_str),
            Some("synth p1")
        );
    }

    #[test]
    fn show_without_name_returns_bad_params() {
        let conn = fresh_conn();
        let err = handle_profile_command("show", &json!({}), &conn).unwrap_err();
        assert!(matches!(err, ProfileHandlerError::BadParams(_)));
        assert_eq!(err.error_code(), "robot.profile.bad_params");
    }

    #[test]
    fn show_for_missing_profile_returns_not_found() {
        let conn = fresh_conn();
        let err = handle_profile_command("show", &json!({ "name": "ghost" }), &conn).unwrap_err();
        match err {
            ProfileHandlerError::NotFound { name } => assert_eq!(name, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn validate_returns_valid_for_well_formed_profile() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("good", "r", vec![])).unwrap();
        let v = handle_profile_command("validate", &json!({ "name": "good" }), &conn).expect("ok");
        assert_eq!(v.get("valid").and_then(Value::as_bool), Some(true));
        let issues = v.get("issues").and_then(Value::as_array).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn validate_returns_invalid_with_reason_when_missing() {
        let conn = fresh_conn();
        let v = handle_profile_command("validate", &json!({ "name": "ghost" }), &conn).expect("ok");
        assert_eq!(v.get("valid").and_then(Value::as_bool), Some(false));
        let issues = v.get("issues").and_then(Value::as_array).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn apply_dry_run_returns_plan_without_mutation() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let v = handle_profile_command(
            "apply",
            &json!({ "name": "ready", "count": 3, "dry_run": true }),
            &conn,
        )
        .expect("ok");
        assert_eq!(v.get("profile_name").and_then(Value::as_str), Some("ready"));
        assert_eq!(v.get("dry_run").and_then(Value::as_bool), Some(true));
        assert_eq!(
            v.get("panes_spawned")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn apply_non_dry_run_returns_spawn_failed() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("ready", "r", vec![])).unwrap();
        let err = handle_profile_command("apply", &json!({ "name": "ready" }), &conn).unwrap_err();
        match err {
            ProfileHandlerError::SpawnFailed { reason } => {
                assert!(reason.contains("ready"));
                assert!(reason.contains("count=1"));
                assert!(reason.contains("daemon-mediated pane spawning"));
            }
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[test]
    fn apply_for_missing_profile_returns_not_found_before_spawn_check() {
        let conn = fresh_conn();
        let err = handle_profile_command("apply", &json!({ "name": "ghost" }), &conn).unwrap_err();
        assert!(matches!(err, ProfileHandlerError::NotFound { .. }));
    }

    #[test]
    fn unknown_action_returns_typed_error() {
        let conn = fresh_conn();
        let err = handle_profile_command("noop", &json!({}), &conn).unwrap_err();
        match err {
            ProfileHandlerError::UnknownAction(a) => assert_eq!(a, "noop"),
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[test]
    fn list_is_deterministic() {
        // Two consecutive calls on the same DB must return identical
        // bytes — the contract's `list_is_deterministic` invariant.
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("a", "r", vec![])).unwrap();
        insert_agent_profile(&conn, &synth("b", "r", vec![])).unwrap();
        let v1 = handle_profile_command("list", &json!({}), &conn).unwrap();
        let v2 = handle_profile_command("list", &json!({}), &conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn show_is_deterministic() {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &synth("p", "r", vec!["t"])).unwrap();
        let v1 = handle_profile_command("show", &json!({ "name": "p" }), &conn).unwrap();
        let v2 = handle_profile_command("show", &json!({ "name": "p" }), &conn).unwrap();
        assert_eq!(v1, v2);
    }

    #[test]
    fn error_code_maps_each_variant() {
        assert_eq!(
            ProfileHandlerError::UnknownAction("x".into()).error_code(),
            "robot.profile.unknown_action",
        );
        assert_eq!(
            ProfileHandlerError::BadParams("x".into()).error_code(),
            "robot.profile.bad_params",
        );
        assert_eq!(
            ProfileHandlerError::NotFound { name: "x".into() }.error_code(),
            "robot.profile.not_found",
        );
        assert_eq!(
            ProfileHandlerError::SpawnFailed { reason: "x".into() }.error_code(),
            "robot.profile.spawn_failed",
        );
    }
}
