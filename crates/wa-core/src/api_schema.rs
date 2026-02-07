//! Schema-driven API strategy: types, versioning, and contracts.
//!
//! # Strategy Decision
//!
//! **Generation direction: Rust structs → JSON Schema.**
//!
//! Rust is the single source of truth for the robot and MCP API.
//! JSON Schema files are *generated* from Rust types (via `schemars`)
//! and committed to `docs/json-schema/` as golden files.  Hand-authored
//! schemas are replaced over time as generation covers each endpoint.
//!
//! Rationale:
//! - Rust already owns the types (~35 output structs in `main.rs`).
//! - The existing 23 hand-authored schemas have no enforcement against
//!   Rust struct drift — a derive-macro approach closes the gap.
//! - `schemars` integrates naturally with `serde` and `Serialize`.
//!
//! # Client targets
//!
//! | Target       | Priority | Approach |
//! |--------------|----------|----------|
//! | Rust crate   | P0       | In-repo `wa-client` crate, types re-exported |
//! | TypeScript   | P1       | Generated from JSON Schema via `json-schema-to-typescript` |
//! | Python       | P2       | Generated from JSON Schema via `datamodel-code-generator` |
//!
//! The Rust client crate is first because it provides compile-time
//! safety for in-repo consumers (tests, MCP delegation, etc.).
//! TypeScript and Python clients are generated offline from the
//! committed JSON Schema files.
//!
//! # Versioning policy
//!
//! - Schema version = wa version (semver, from `Cargo.toml`).
//! - Each generated schema file includes `$id` with the version.
//! - Breaking changes: detected by diffing schemas between versions.
//!   A breaking change bumps the wa **minor** version (pre-1.0)
//!   or **major** version (post-1.0).
//! - The MCP surface keeps its own `mcp_version` field ("v1", "v2", …)
//!   for protocol-level compatibility.  Schema versioning is orthogonal.
//! - CI validates: `cargo test --test schema_golden` diffs generated
//!   schemas against committed golden files, failing if they diverge
//!   without an explicit version bump.
//!
//! # Implementation path
//!
//! 1. Add `schemars` derive to robot output types (wa-upg.10.2)
//! 2. Generate `docs/json-schema/` from Rust types, replace hand-authored
//! 3. Add golden-file CI test
//! 4. Create `wa-client` crate re-exporting the types (wa-upg.10.3)
//! 5. Add TS/Python generation scripts (wa-upg.10.3)

use serde::{Deserialize, Serialize};

// ───────────────────────────────────────────────────────────────────────────
// API version
// ───────────────────────────────────────────────────────────────────────────

/// API schema version (tracks wa semver).
///
/// The schema version is embedded in generated JSON Schema `$id` URLs
/// and in the robot response `version` field.  Client libraries can
/// check compatibility before parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ApiVersion {
    /// Parse a semver string like "0.1.0" into an `ApiVersion`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() < 3 {
            return None;
        }
        Some(Self {
            major: parts[0].parse().ok()?,
            minor: parts[1].parse().ok()?,
            patch: parts[2].parse().ok()?,
        })
    }

    /// Current API version from the crate version.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(crate::VERSION).expect("Cargo.toml version is valid semver")
    }

    /// True when this version can read data produced by `wire_version`.
    ///
    /// Pre-1.0: same major + minor (patch changes are always compatible).
    /// Post-1.0: same major (minor additions are backwards-compatible).
    #[must_use]
    pub fn is_compatible_with(&self, wire_version: &Self) -> bool {
        if self.major == 0 {
            // Pre-1.0: minor bumps are breaking
            self.major == wire_version.major && self.minor == wire_version.minor
        } else {
            // Post-1.0: same major is compatible
            self.major == wire_version.major
        }
    }

    /// Classify the difference between this reader and a wire version.
    #[must_use]
    pub fn compatibility(&self, wire_version: &Self) -> VersionCompatibility {
        if self == wire_version {
            return VersionCompatibility::Exact;
        }
        if self.major != wire_version.major {
            return VersionCompatibility::Incompatible;
        }
        if self.major == 0 {
            // Pre-1.0: minor bump = breaking
            if self.minor != wire_version.minor {
                return VersionCompatibility::Incompatible;
            }
            // Same major.minor, different patch → compatible
            VersionCompatibility::Compatible
        } else {
            // Post-1.0: same major, different minor/patch
            if wire_version.minor > self.minor {
                VersionCompatibility::NewerMinor
            } else {
                VersionCompatibility::Compatible
            }
        }
    }
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Result of comparing a reader's version with a wire version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCompatibility {
    /// Versions are identical.
    Exact,
    /// Versions are compatible (same major, reader can handle wire).
    Compatible,
    /// Wire has a newer minor — reader may miss optional fields.
    NewerMinor,
    /// Versions are incompatible (different major, or pre-1.0 minor drift).
    Incompatible,
}

// ───────────────────────────────────────────────────────────────────────────
// API endpoint registry
// ───────────────────────────────────────────────────────────────────────────

/// Metadata describing a single robot/MCP API endpoint.
///
/// Each robot subcommand and MCP tool that produces structured output
/// should have a corresponding entry.  This is used to:
/// - Generate JSON Schema files (`docs/json-schema/`)
/// - Produce reference documentation pages
/// - Enforce coverage (every endpoint has a schema)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointMeta {
    /// Machine-readable identifier (e.g., "state", "get_text", "search").
    pub id: String,
    /// Human-readable title (e.g., "Get Pane Text").
    pub title: String,
    /// Short description for docs.
    pub description: String,
    /// Robot subcommand name (e.g., "robot state").
    pub robot_command: Option<String>,
    /// MCP tool name (e.g., "wa.state").
    pub mcp_tool: Option<String>,
    /// Schema filename in `docs/json-schema/` (e.g., "wa-robot-state.json").
    pub schema_file: String,
    /// Whether this endpoint is stable (false = experimental).
    pub stable: bool,
    /// Minimum wa version where this endpoint was introduced.
    pub since: String,
}

/// Registry of all known API endpoints.
///
/// The registry is the authoritative list used for schema generation,
/// docs generation, and coverage checks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaRegistry {
    /// API version this registry describes.
    pub version: String,
    /// All registered endpoints.
    pub endpoints: Vec<EndpointMeta>,
}

impl SchemaRegistry {
    /// Build the canonical registry of all wa robot/MCP endpoints.
    ///
    /// This is the single source of truth for what endpoints exist.
    /// When adding a new robot command or MCP tool, add it here.
    #[must_use]
    pub fn canonical() -> Self {
        Self {
            version: crate::VERSION.to_string(),
            endpoints: vec![
                EndpointMeta {
                    id: "help".into(),
                    title: "Robot Help".into(),
                    description: "List robot commands and flags".into(),
                    robot_command: Some("robot help".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-help.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "state".into(),
                    title: "Pane State".into(),
                    description: "Get all observed panes as structured data".into(),
                    robot_command: Some("robot state".into()),
                    mcp_tool: Some("wa.state".into()),
                    schema_file: "wa-robot-state.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "get_text".into(),
                    title: "Get Pane Text".into(),
                    description: "Extract text from a specific pane".into(),
                    robot_command: Some("robot get-text".into()),
                    mcp_tool: Some("wa.get_text".into()),
                    schema_file: "wa-robot-get-text.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "send".into(),
                    title: "Send Text".into(),
                    description: "Send text to a pane".into(),
                    robot_command: Some("robot send".into()),
                    mcp_tool: Some("wa.send".into()),
                    schema_file: "wa-robot-send.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "wait_for".into(),
                    title: "Wait For Pattern".into(),
                    description: "Wait for a pattern to appear in pane output".into(),
                    robot_command: Some("robot wait-for".into()),
                    mcp_tool: Some("wa.wait_for".into()),
                    schema_file: "wa-robot-wait-for.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "search".into(),
                    title: "Search".into(),
                    description: "Full-text search captured output".into(),
                    robot_command: Some("robot search".into()),
                    mcp_tool: Some("wa.search".into()),
                    schema_file: "wa-robot-search.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "events".into(),
                    title: "Events".into(),
                    description: "Query recent events with filtering".into(),
                    robot_command: Some("robot events".into()),
                    mcp_tool: Some("wa.events".into()),
                    schema_file: "wa-robot-events.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "events_annotate".into(),
                    title: "Annotate Event".into(),
                    description: "Set or clear notes on an event".into(),
                    robot_command: Some("robot events annotate".into()),
                    mcp_tool: Some("wa.events_annotate".into()),
                    schema_file: "wa-robot-event-mutation.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "events_triage".into(),
                    title: "Triage Event".into(),
                    description: "Set or clear triage state on an event".into(),
                    robot_command: Some("robot events triage".into()),
                    mcp_tool: Some("wa.events_triage".into()),
                    schema_file: "wa-robot-event-mutation.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "events_label".into(),
                    title: "Label Event".into(),
                    description: "Add or remove labels on an event".into(),
                    robot_command: Some("robot events label".into()),
                    mcp_tool: Some("wa.events_label".into()),
                    schema_file: "wa-robot-event-mutation.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "workflow_run".into(),
                    title: "Run Workflow".into(),
                    description: "Execute a named workflow".into(),
                    robot_command: Some("robot workflow run".into()),
                    mcp_tool: Some("wa.workflow_run".into()),
                    schema_file: "wa-robot-workflow-run.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "workflow_list".into(),
                    title: "List Workflows".into(),
                    description: "List available workflows".into(),
                    robot_command: Some("robot workflow list".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-workflow-list.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "workflow_status".into(),
                    title: "Workflow Status".into(),
                    description: "Check workflow execution status".into(),
                    robot_command: Some("robot workflow status".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-workflow-status.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "workflow_abort".into(),
                    title: "Abort Workflow".into(),
                    description: "Abort a running workflow".into(),
                    robot_command: Some("robot workflow abort".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-workflow-abort.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "rules_list".into(),
                    title: "List Rules".into(),
                    description: "List detection rules".into(),
                    robot_command: Some("robot rules list".into()),
                    mcp_tool: Some("wa.rules_list".into()),
                    schema_file: "wa-robot-rules-list.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "rules_test".into(),
                    title: "Test Rules".into(),
                    description: "Test text against detection rules".into(),
                    robot_command: Some("robot rules test".into()),
                    mcp_tool: Some("wa.rules_test".into()),
                    schema_file: "wa-robot-rules-test.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "rules_show".into(),
                    title: "Show Rule".into(),
                    description: "Show full rule details".into(),
                    robot_command: Some("robot rules show".into()),
                    mcp_tool: Some("wa.rules_show".into()),
                    schema_file: "wa-robot-rules-show.json".into(),
                    stable: false,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "rules_lint".into(),
                    title: "Lint Rules".into(),
                    description: "Validate rule definitions".into(),
                    robot_command: Some("robot rules lint".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-rules-lint.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "accounts_list".into(),
                    title: "List Accounts".into(),
                    description: "List configured accounts".into(),
                    robot_command: Some("robot accounts list".into()),
                    mcp_tool: Some("wa.accounts".into()),
                    schema_file: "wa-robot-accounts.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "accounts_refresh".into(),
                    title: "Refresh Accounts".into(),
                    description: "Refresh account usage metrics".into(),
                    robot_command: Some("robot accounts refresh".into()),
                    mcp_tool: Some("wa.accounts_refresh".into()),
                    schema_file: "wa-robot-accounts-refresh.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "reservations_list".into(),
                    title: "List Reservations".into(),
                    description: "List active pane reservations".into(),
                    robot_command: Some("robot reservations list".into()),
                    mcp_tool: Some("wa.reservations".into()),
                    schema_file: "wa-robot-reservations.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "reserve".into(),
                    title: "Reserve Pane".into(),
                    description: "Create a pane reservation".into(),
                    robot_command: Some("robot reserve".into()),
                    mcp_tool: Some("wa.reserve".into()),
                    schema_file: "wa-robot-reserve.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "release".into(),
                    title: "Release Reservation".into(),
                    description: "Release a pane reservation".into(),
                    robot_command: Some("robot release".into()),
                    mcp_tool: Some("wa.release".into()),
                    schema_file: "wa-robot-release.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "approve".into(),
                    title: "Submit Approval".into(),
                    description: "Submit an approval code".into(),
                    robot_command: Some("robot approve".into()),
                    mcp_tool: Some("wa.approve".into()),
                    schema_file: "wa-robot-approve.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "why".into(),
                    title: "Explain Error".into(),
                    description: "Explain an error code or policy denial".into(),
                    robot_command: Some("robot why".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-why.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
            ],
        }
    }

    /// Find an endpoint by its id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&EndpointMeta> {
        self.endpoints.iter().find(|e| e.id == id)
    }

    /// All endpoint ids.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.endpoints.iter().map(|e| e.id.as_str())
    }

    /// Endpoints that have both robot and MCP surfaces.
    pub fn dual_surface(&self) -> impl Iterator<Item = &EndpointMeta> {
        self.endpoints
            .iter()
            .filter(|e| e.robot_command.is_some() && e.mcp_tool.is_some())
    }

    /// Endpoints that only have a robot surface (no MCP tool).
    pub fn robot_only(&self) -> impl Iterator<Item = &EndpointMeta> {
        self.endpoints
            .iter()
            .filter(|e| e.robot_command.is_some() && e.mcp_tool.is_none())
    }

    /// Unique schema filenames referenced by endpoints.
    pub fn schema_files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self
            .endpoints
            .iter()
            .map(|e| e.schema_file.as_str())
            .collect();
        files.sort();
        files.dedup();
        files
    }

    /// Check whether all existing `docs/json-schema/` files are covered.
    ///
    /// Returns schema filenames that exist on disk but are NOT in the registry.
    #[must_use]
    pub fn uncovered_schemas(&self, schema_dir_files: &[String]) -> Vec<String> {
        let registered: std::collections::HashSet<&str> = self
            .endpoints
            .iter()
            .map(|e| e.schema_file.as_str())
            .collect();
        schema_dir_files
            .iter()
            .filter(|f| !registered.contains(f.as_str()))
            .cloned()
            .collect()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Schema diff (breaking-change detection)
// ───────────────────────────────────────────────────────────────────────────

/// A schema change between two versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaChange {
    /// Schema file that changed.
    pub schema_file: String,
    /// Kind of change.
    pub kind: ChangeKind,
    /// Human-readable description.
    pub description: String,
}

/// Classification of a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// New schema file added (backwards-compatible).
    Added,
    /// Schema file removed (breaking).
    Removed,
    /// Required field added to response (breaking for existing clients).
    RequiredFieldAdded,
    /// Optional field added to response (backwards-compatible).
    OptionalFieldAdded,
    /// Field removed from response (breaking).
    FieldRemoved,
    /// Field type changed (breaking).
    TypeChanged,
    /// Non-structural change (description, title, etc.).
    Cosmetic,
}

impl ChangeKind {
    /// Whether this change kind is breaking for existing clients.
    #[must_use]
    pub fn is_breaking(&self) -> bool {
        matches!(
            self,
            Self::Removed | Self::RequiredFieldAdded | Self::FieldRemoved | Self::TypeChanged
        )
    }
}

/// Result of comparing schemas between two versions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaDiffResult {
    /// Version being compared from.
    pub from_version: String,
    /// Version being compared to.
    pub to_version: String,
    /// All detected changes.
    pub changes: Vec<SchemaChange>,
}

impl SchemaDiffResult {
    /// True if any change is breaking.
    #[must_use]
    pub fn has_breaking_changes(&self) -> bool {
        self.changes.iter().any(|c| c.kind.is_breaking())
    }

    /// Only the breaking changes.
    pub fn breaking_changes(&self) -> impl Iterator<Item = &SchemaChange> {
        self.changes.iter().filter(|c| c.kind.is_breaking())
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- ApiVersion ---

    #[test]
    fn parse_valid_version() {
        let v = ApiVersion::parse("0.1.0").unwrap();
        assert_eq!(
            v,
            ApiVersion {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_invalid_version() {
        assert!(ApiVersion::parse("invalid").is_none());
        assert!(ApiVersion::parse("1.2").is_none());
        assert!(ApiVersion::parse("").is_none());
    }

    #[test]
    fn current_version_parses() {
        let v = ApiVersion::current();
        assert_eq!(v.to_string(), crate::VERSION);
    }

    #[test]
    fn version_display() {
        let v = ApiVersion {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert_eq!(v.to_string(), "1.2.3");
    }

    #[test]
    fn version_roundtrip_serde() {
        let v = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        let json = serde_json::to_string(&v).unwrap();
        let parsed: ApiVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, parsed);
    }

    // --- Pre-1.0 compatibility ---

    #[test]
    fn pre1_same_version_exact() {
        let v = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        assert_eq!(v.compatibility(&v), VersionCompatibility::Exact);
        assert!(v.is_compatible_with(&v));
    }

    #[test]
    fn pre1_same_minor_different_patch_compatible() {
        let reader = ApiVersion {
            major: 0,
            minor: 1,
            patch: 2,
        };
        let wire = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::Compatible
        );
        assert!(reader.is_compatible_with(&wire));
    }

    #[test]
    fn pre1_different_minor_incompatible() {
        let reader = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        let wire = ApiVersion {
            major: 0,
            minor: 2,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::Incompatible
        );
        assert!(!reader.is_compatible_with(&wire));
    }

    #[test]
    fn pre1_different_major_incompatible() {
        let reader = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        let wire = ApiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::Incompatible
        );
    }

    // --- Post-1.0 compatibility ---

    #[test]
    fn post1_same_major_compatible() {
        let reader = ApiVersion {
            major: 1,
            minor: 2,
            patch: 0,
        };
        let wire = ApiVersion {
            major: 1,
            minor: 1,
            patch: 5,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::Compatible
        );
        assert!(reader.is_compatible_with(&wire));
    }

    #[test]
    fn post1_newer_minor_warns() {
        let reader = ApiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        };
        let wire = ApiVersion {
            major: 1,
            minor: 3,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::NewerMinor
        );
        // Still compatible at major level
        assert!(reader.is_compatible_with(&wire));
    }

    #[test]
    fn post1_different_major_incompatible() {
        let reader = ApiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        };
        let wire = ApiVersion {
            major: 2,
            minor: 0,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire),
            VersionCompatibility::Incompatible
        );
        assert!(!reader.is_compatible_with(&wire));
    }

    // --- SchemaRegistry ---

    #[test]
    fn canonical_registry_is_nonempty() {
        let reg = SchemaRegistry::canonical();
        assert!(!reg.endpoints.is_empty());
    }

    #[test]
    fn canonical_registry_has_version() {
        let reg = SchemaRegistry::canonical();
        assert_eq!(reg.version, crate::VERSION);
    }

    #[test]
    fn canonical_registry_ids_are_unique() {
        let reg = SchemaRegistry::canonical();
        let ids: Vec<&str> = reg.ids().collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "duplicate endpoint ids");
    }

    #[test]
    fn canonical_registry_get_works() {
        let reg = SchemaRegistry::canonical();
        let state = reg.get("state").expect("state endpoint exists");
        assert_eq!(state.title, "Pane State");
        assert!(state.stable);
    }

    #[test]
    fn canonical_registry_get_missing_returns_none() {
        let reg = SchemaRegistry::canonical();
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn dual_surface_endpoints_have_both() {
        let reg = SchemaRegistry::canonical();
        for ep in reg.dual_surface() {
            assert!(
                ep.robot_command.is_some(),
                "{} missing robot_command",
                ep.id
            );
            assert!(ep.mcp_tool.is_some(), "{} missing mcp_tool", ep.id);
        }
    }

    #[test]
    fn robot_only_endpoints_have_no_mcp() {
        let reg = SchemaRegistry::canonical();
        for ep in reg.robot_only() {
            assert!(ep.mcp_tool.is_none(), "{} has unexpected mcp_tool", ep.id);
        }
    }

    #[test]
    fn schema_files_are_nonempty() {
        let reg = SchemaRegistry::canonical();
        let files = reg.schema_files();
        assert!(!files.is_empty());
        for f in &files {
            assert!(
                std::path::Path::new(f)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json")),
                "schema file should be .json: {f}"
            );
        }
    }

    #[test]
    fn uncovered_schemas_detects_unknown() {
        let reg = SchemaRegistry::canonical();
        let on_disk = vec![
            "wa-robot-state.json".to_string(),
            "wa-robot-foo.json".to_string(),
        ];
        let uncovered = reg.uncovered_schemas(&on_disk);
        assert_eq!(uncovered, vec!["wa-robot-foo.json"]);
    }

    #[test]
    fn uncovered_schemas_empty_when_all_covered() {
        let reg = SchemaRegistry::canonical();
        let on_disk: Vec<String> = reg.schema_files().iter().map(|s| s.to_string()).collect();
        let uncovered = reg.uncovered_schemas(&on_disk);
        assert!(uncovered.is_empty());
    }

    #[test]
    fn registry_roundtrip_serde() {
        let reg = SchemaRegistry::canonical();
        let json = serde_json::to_string(&reg).unwrap();
        let parsed: SchemaRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.endpoints.len(), reg.endpoints.len());
    }

    // --- ChangeKind ---

    #[test]
    fn breaking_changes_are_classified() {
        assert!(ChangeKind::Removed.is_breaking());
        assert!(ChangeKind::RequiredFieldAdded.is_breaking());
        assert!(ChangeKind::FieldRemoved.is_breaking());
        assert!(ChangeKind::TypeChanged.is_breaking());
        assert!(!ChangeKind::Added.is_breaking());
        assert!(!ChangeKind::OptionalFieldAdded.is_breaking());
        assert!(!ChangeKind::Cosmetic.is_breaking());
    }

    #[test]
    fn schema_diff_breaking_detection() {
        let diff = SchemaDiffResult {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            changes: vec![SchemaChange {
                schema_file: "wa-robot-state.json".into(),
                kind: ChangeKind::OptionalFieldAdded,
                description: "Added new optional field".into(),
            }],
        };
        assert!(!diff.has_breaking_changes());

        let diff_breaking = SchemaDiffResult {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            changes: vec![SchemaChange {
                schema_file: "wa-robot-state.json".into(),
                kind: ChangeKind::FieldRemoved,
                description: "Removed field X".into(),
            }],
        };
        assert!(diff_breaking.has_breaking_changes());
        assert_eq!(diff_breaking.breaking_changes().count(), 1);
    }

    #[test]
    fn schema_diff_roundtrip_serde() {
        let diff = SchemaDiffResult {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            changes: vec![SchemaChange {
                schema_file: "wa-robot-state.json".into(),
                kind: ChangeKind::Added,
                description: "New schema".into(),
            }],
        };
        let json = serde_json::to_string(&diff).unwrap();
        let parsed: SchemaDiffResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.changes.len(), 1);
        assert_eq!(parsed.changes[0].kind, ChangeKind::Added);
    }

    // --- Coverage check against actual schema files ---

    #[test]
    fn registry_covers_existing_schemas() {
        let reg = SchemaRegistry::canonical();
        let registered: std::collections::HashSet<&str> = reg.schema_files().into_iter().collect();

        // These are the known hand-authored schemas that should be covered
        let expected = [
            "wa-robot-help.json",
            "wa-robot-state.json",
            "wa-robot-get-text.json",
            "wa-robot-send.json",
            "wa-robot-wait-for.json",
            "wa-robot-search.json",
            "wa-robot-events.json",
            "wa-robot-event-mutation.json",
            "wa-robot-workflow-run.json",
            "wa-robot-workflow-list.json",
            "wa-robot-workflow-status.json",
            "wa-robot-workflow-abort.json",
            "wa-robot-rules-list.json",
            "wa-robot-rules-test.json",
            "wa-robot-rules-lint.json",
            "wa-robot-accounts.json",
            "wa-robot-accounts-refresh.json",
            "wa-robot-reservations.json",
            "wa-robot-reserve.json",
            "wa-robot-release.json",
            "wa-robot-approve.json",
            "wa-robot-why.json",
        ];

        for schema in &expected {
            assert!(
                registered.contains(schema),
                "hand-authored schema {schema} is not in the registry"
            );
        }
    }
}
