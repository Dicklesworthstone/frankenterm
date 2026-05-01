//! `agent_profiles` table substrate — typed shape + schema +
//! validation for the robot-mode profile family handler.
//!
//! **Bead:** [BR-RC-ROBOT-CONTRACT.1.cont.handler] / `ft-df3cz`.
//! **Parent:** ft-hac7w.2 (profile family contract — closed).
//! **State-machine model:** [`crate::robot_profile_state_machine`].
//! **Contract doc:** `docs/robot-contracts/profile.md`.
//!
//! # What this module ships (substrate-pass)
//!
//! - [`AgentProfile`] — typed shape matching the bead's stated
//!   schema: `id, name, role, tags, shell, command, env,
//!   metadata, created_at_ms, updated_at_ms`.
//! - [`AGENT_PROFILES_SCHEMA`] — the `CREATE TABLE` SQL the
//!   migration runner consumes.
//! - [`AgentProfile::validate`] — name + role + length
//!   invariants per the contract doc:
//!   - `name` matches `/^[a-zA-Z0-9_-]+$/`, 1..=64 chars.
//!   - `role` 0..=64 chars (empty allowed).
//!   - tag values 1..=64 chars each.
//!
//! # What this module does NOT ship (wired-pass follow-ups)
//!
//! - The actual storage.rs migration step that registers
//!   `AGENT_PROFILES_SCHEMA` against the schema_migrations table.
//!   Filed as `ft-df3cz.cont.migration_step` (this commit files
//!   that bead).
//! - The `RobotCommands::Profile` handler at
//!   `crates/frankenterm/src/main.rs:23227` that replaces the
//!   `build_ntm_not_implemented_response` fallback. Filed as
//!   `ft-df3cz.cont.handler_integration`.
//! - Mux integration for `Apply` (spawning `count` panes via
//!   wezterm.rs). Filed alongside cont.handler_integration.
//! - Idempotency check via ApplyReceipt content-hash. Same.
//!
//! Substrate-pass / wired-pass split mirrors the rest of this
//! session's work (ft-t9a6q.1 / ft-hac7w.2 / wa-2l27x.8 / etc.).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `CREATE TABLE` SQL for the `agent_profiles` table. Consumed
/// by the storage.rs migration runner under
/// `ft-df3cz.cont.migration_step`.
///
/// Schema rationale:
/// - `name` is the primary key (TEXT, unique). Profile names
///   are operator-facing identifiers and must be globally
///   unique within an ft installation.
/// - `tags` and `env` are stored as JSON-encoded TEXT to match
///   storage.rs's existing convention for variadic-shape data
///   (see e.g. workflow metadata in storage.rs).
/// - `created_at_ms` / `updated_at_ms` are unix epoch ms `INTEGER`
///   matching storage.rs's hot-path timestamp convention.
pub const AGENT_PROFILES_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS agent_profiles (
    name           TEXT PRIMARY KEY NOT NULL,
    role           TEXT NOT NULL DEFAULT '',
    tags           TEXT NOT NULL DEFAULT '[]',
    shell          TEXT NOT NULL DEFAULT '',
    command        TEXT,
    env            TEXT NOT NULL DEFAULT '{}',
    metadata       TEXT NOT NULL DEFAULT '{}',
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
)";

/// Index on `role` for `list --role <role>` filtering.
pub const AGENT_PROFILES_ROLE_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS agent_profiles_role_idx ON agent_profiles(role)";

/// Maximum length for the `name` field (per contract doc).
pub const NAME_MAX_LEN: usize = 64;

/// Maximum length for the `role` field.
pub const ROLE_MAX_LEN: usize = 64;

/// Maximum length for an individual tag.
pub const TAG_MAX_LEN: usize = 64;

/// Typed agent profile row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl AgentProfile {
    /// Validate the profile's invariants per the contract doc.
    /// Returns `Ok(())` if valid; otherwise the first violated
    /// invariant.
    pub fn validate(&self) -> Result<(), ProfileValidationError> {
        if self.name.is_empty() {
            return Err(ProfileValidationError::NameEmpty);
        }
        if self.name.len() > NAME_MAX_LEN {
            return Err(ProfileValidationError::NameTooLong {
                len: self.name.len(),
                max: NAME_MAX_LEN,
            });
        }
        for (i, c) in self.name.chars().enumerate() {
            if !is_name_char(c) {
                return Err(ProfileValidationError::NameBadChar {
                    position: i,
                    observed: c,
                });
            }
        }
        if self.role.len() > ROLE_MAX_LEN {
            return Err(ProfileValidationError::RoleTooLong {
                len: self.role.len(),
                max: ROLE_MAX_LEN,
            });
        }
        for (i, tag) in self.tags.iter().enumerate() {
            if tag.is_empty() {
                return Err(ProfileValidationError::TagEmpty { position: i });
            }
            if tag.len() > TAG_MAX_LEN {
                return Err(ProfileValidationError::TagTooLong {
                    position: i,
                    len: tag.len(),
                    max: TAG_MAX_LEN,
                });
            }
        }
        if self.created_at_ms < 0 {
            return Err(ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                value: self.created_at_ms,
            });
        }
        if self.updated_at_ms < 0 {
            return Err(ProfileValidationError::TimestampNegative {
                field: "updated_at_ms",
                value: self.updated_at_ms,
            });
        }
        if self.updated_at_ms < self.created_at_ms {
            return Err(ProfileValidationError::UpdatedBeforeCreated {
                created: self.created_at_ms,
                updated: self.updated_at_ms,
            });
        }
        Ok(())
    }
}

/// Validation error taxonomy. Each variant maps to a specific
/// invariant the handler enforces before persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileValidationError {
    NameEmpty,
    NameTooLong {
        len: usize,
        max: usize,
    },
    NameBadChar {
        position: usize,
        observed: char,
    },
    RoleTooLong {
        len: usize,
        max: usize,
    },
    TagEmpty {
        position: usize,
    },
    TagTooLong {
        position: usize,
        len: usize,
        max: usize,
    },
    TimestampNegative {
        field: &'static str,
        value: i64,
    },
    UpdatedBeforeCreated {
        created: i64,
        updated: i64,
    },
}

impl std::fmt::Display for ProfileValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameEmpty => write!(f, "profile name must not be empty"),
            Self::NameTooLong { len, max } => {
                write!(f, "profile name length {len} exceeds maximum {max}")
            }
            Self::NameBadChar { position, observed } => {
                write!(
                    f,
                    "profile name has invalid character {observed:?} at position {position}; \
                     name must match /^[a-zA-Z0-9_-]+$/"
                )
            }
            Self::RoleTooLong { len, max } => {
                write!(f, "profile role length {len} exceeds maximum {max}")
            }
            Self::TagEmpty { position } => {
                write!(f, "profile tag at position {position} is empty")
            }
            Self::TagTooLong { position, len, max } => {
                write!(
                    f,
                    "profile tag at position {position} has length {len}, exceeds maximum {max}"
                )
            }
            Self::TimestampNegative { field, value } => {
                write!(f, "profile {field} is negative: {value}")
            }
            Self::UpdatedBeforeCreated { created, updated } => {
                write!(
                    f,
                    "profile updated_at_ms ({updated}) precedes created_at_ms ({created})"
                )
            }
        }
    }
}

impl std::error::Error for ProfileValidationError {}

/// Whether `c` is allowed in a profile name (regex: `[a-zA-Z0-9_-]`).
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> AgentProfile {
        AgentProfile {
            name: "cc-agent".to_string(),
            role: "engineer".to_string(),
            tags: vec!["main".to_string(), "default".to_string()],
            shell: "/bin/zsh".to_string(),
            command: Some("claude".to_string()),
            env: {
                let mut m = HashMap::new();
                m.insert("ANTHROPIC_API_KEY".to_string(), "sk-...".to_string());
                m
            },
            metadata: HashMap::new(),
            created_at_ms: 1_715_000_000_000,
            updated_at_ms: 1_715_000_000_000,
        }
    }

    #[test]
    fn schema_constant_is_well_formed_create_table() {
        assert!(AGENT_PROFILES_SCHEMA.starts_with("CREATE TABLE"));
        assert!(AGENT_PROFILES_SCHEMA.contains("agent_profiles"));
        assert!(AGENT_PROFILES_SCHEMA.contains("name"));
        assert!(AGENT_PROFILES_SCHEMA.contains("role"));
        assert!(AGENT_PROFILES_SCHEMA.contains("tags"));
        assert!(AGENT_PROFILES_SCHEMA.contains("created_at_ms"));
        assert!(AGENT_PROFILES_SCHEMA.contains("updated_at_ms"));
    }

    #[test]
    fn schema_uses_if_not_exists_for_idempotent_migration() {
        // The migration runner re-runs the schema on every
        // launch; IF NOT EXISTS makes that safe.
        assert!(AGENT_PROFILES_SCHEMA.contains("IF NOT EXISTS"));
        assert!(AGENT_PROFILES_ROLE_INDEX.contains("IF NOT EXISTS"));
    }

    #[test]
    fn role_index_targets_role_column() {
        assert!(AGENT_PROFILES_ROLE_INDEX.contains("agent_profiles(role)"));
    }

    #[test]
    fn validate_accepts_a_well_formed_profile() {
        let p = sample_profile();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut p = sample_profile();
        p.name = String::new();
        assert_eq!(p.validate(), Err(ProfileValidationError::NameEmpty));
    }

    #[test]
    fn validate_rejects_name_over_max_length() {
        let mut p = sample_profile();
        p.name = "a".repeat(NAME_MAX_LEN + 1);
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_name_with_bad_char() {
        let mut p = sample_profile();
        p.name = "bad name".to_string(); // space not allowed
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameBadChar {
                observed: ' ',
                ..
            })
        ));
    }

    #[test]
    fn validate_accepts_alphanumeric_underscore_hyphen() {
        let names = [
            "simple",
            "with_underscore",
            "with-hyphen",
            "Mixed123",
            "all-the_characters-1234",
        ];
        for name in &names {
            let mut p = sample_profile();
            p.name = (*name).to_string();
            assert_eq!(
                p.validate(),
                Ok(()),
                "name {name:?} should validate"
            );
        }
    }

    #[test]
    fn validate_rejects_role_over_max_length() {
        let mut p = sample_profile();
        p.role = "x".repeat(ROLE_MAX_LEN + 1);
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::RoleTooLong { .. })
        ));
    }

    #[test]
    fn validate_accepts_empty_role() {
        let mut p = sample_profile();
        p.role = String::new();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_empty_tag() {
        let mut p = sample_profile();
        p.tags = vec!["good".to_string(), String::new(), "also_good".to_string()];
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TagEmpty { position: 1 })
        ));
    }

    #[test]
    fn validate_rejects_oversize_tag() {
        let mut p = sample_profile();
        p.tags = vec!["x".repeat(TAG_MAX_LEN + 1)];
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TagTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_negative_timestamps() {
        let mut p = sample_profile();
        p.created_at_ms = -1;
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                ..
            })
        ));
    }

    #[test]
    fn validate_rejects_updated_before_created() {
        let mut p = sample_profile();
        p.created_at_ms = 1000;
        p.updated_at_ms = 500;
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::UpdatedBeforeCreated { .. })
        ));
    }

    #[test]
    fn validation_error_renders_each_variant() {
        let errs = vec![
            ProfileValidationError::NameEmpty,
            ProfileValidationError::NameTooLong { len: 100, max: 64 },
            ProfileValidationError::NameBadChar {
                position: 3,
                observed: '!',
            },
            ProfileValidationError::RoleTooLong { len: 100, max: 64 },
            ProfileValidationError::TagEmpty { position: 0 },
            ProfileValidationError::TagTooLong {
                position: 0,
                len: 100,
                max: 64,
            },
            ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                value: -1,
            },
            ProfileValidationError::UpdatedBeforeCreated {
                created: 1000,
                updated: 500,
            },
        ];
        for err in &errs {
            let s = err.to_string();
            assert!(!s.is_empty(), "Display for {err:?} produced empty string");
        }
    }

    #[test]
    fn serde_round_trips_a_profile() {
        let p = sample_profile();
        let json = serde_json::to_string(&p).unwrap();
        let decoded: AgentProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn serde_defaults_apply_when_optional_fields_are_omitted() {
        // Minimal JSON — only required fields.
        let json = r#"{
            "name": "minimal",
            "created_at_ms": 0,
            "updated_at_ms": 0
        }"#;
        let p: AgentProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.name, "minimal");
        assert_eq!(p.role, "");
        assert!(p.tags.is_empty());
        assert_eq!(p.shell, "");
        assert_eq!(p.command, None);
        assert!(p.env.is_empty());
        assert!(p.metadata.is_empty());
    }
}
