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
//! - Schema version = ft version (semver, from `Cargo.toml`).
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
    /// Minimum ft version where this endpoint was introduced.
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
    /// Build the canonical registry of all ft robot/MCP endpoints.
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
                    id: "quickstart".into(),
                    title: "Robot Quick Start".into(),
                    description: "Show the machine-oriented quick-start guide".into(),
                    robot_command: Some("robot quick-start".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-quickstart.json".into(),
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
                    id: "agents_list".into(),
                    title: "List Agents".into(),
                    description: "List installed agents from filesystem detection".into(),
                    robot_command: Some("robot agents list".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-agents-list.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "agents_running".into(),
                    title: "List Running Agents".into(),
                    description: "List running agents inferred from active panes".into(),
                    robot_command: Some("robot agents running".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-agents-running.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "agents_detect".into(),
                    title: "Detect Agents".into(),
                    description: "Run installed-agent detection and return inventory".into(),
                    robot_command: Some("robot agents detect".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-agents-detect.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "agents_configure".into(),
                    title: "Configure Agents".into(),
                    description:
                        "Generate or update FrankenTerm integration config files for detected agents"
                            .into(),
                    robot_command: Some("robot agents configure".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-agents-configure.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "agents_configure_dry_run".into(),
                    title: "Configure Agents Dry-Run".into(),
                    description:
                        "Preview FrankenTerm integration config generation for detected agents"
                            .into(),
                    robot_command: Some("robot agents configure --dry-run".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-agents-configure-dry-run.json".into(),
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
                    robot_command: Some("robot reservations reserve".into()),
                    mcp_tool: Some("wa.reserve".into()),
                    schema_file: "wa-robot-reserve.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "release".into(),
                    title: "Release Reservation".into(),
                    description: "Release a pane reservation".into(),
                    robot_command: Some("robot reservations release".into()),
                    mcp_tool: Some("wa.release".into()),
                    schema_file: "wa-robot-release.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "search_explain".into(),
                    title: "Search Explain".into(),
                    description: "Explain why search results may be missing or incomplete".into(),
                    robot_command: Some("robot search-explain".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-search-explain.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "search_index_stats".into(),
                    title: "Search Index Stats".into(),
                    description: "Show search index size, freshness, and health metrics".into(),
                    robot_command: Some("robot search-index stats".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-search-index-stats.json".into(),
                    stable: true,
                    since: "0.1.0".into(),
                },
                EndpointMeta {
                    id: "search_index_reindex".into(),
                    title: "Search Index Reindex".into(),
                    description: "Rebuild search index from captured segments and pane metadata"
                        .into(),
                    robot_command: Some("robot search-index reindex".into()),
                    mcp_tool: None,
                    schema_file: "wa-robot-search-index-reindex.json".into(),
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
            "wa-robot-quickstart.json",
            "wa-robot-state.json",
            "wa-robot-get-text.json",
            "wa-robot-send.json",
            "wa-robot-wait-for.json",
            "wa-robot-search.json",
            "wa-robot-search-explain.json",
            "wa-robot-search-index-stats.json",
            "wa-robot-search-index-reindex.json",
            "wa-robot-events.json",
            "wa-robot-event-mutation.json",
            "wa-robot-agents-list.json",
            "wa-robot-agents-running.json",
            "wa-robot-agents-detect.json",
            "wa-robot-agents-configure.json",
            "wa-robot-agents-configure-dry-run.json",
            "wa-robot-workflow-run.json",
            "wa-robot-workflow-list.json",
            "wa-robot-workflow-status.json",
            "wa-robot-workflow-abort.json",
            "wa-robot-rules-list.json",
            "wa-robot-rules-test.json",
            "wa-robot-rules-show.json",
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

    #[test]
    fn registered_schema_files_exist_on_disk() {
        let reg = SchemaRegistry::canonical();
        let schema_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/json-schema");

        for schema in reg.schema_files() {
            let path = schema_dir.join(schema);
            assert!(
                path.exists(),
                "registered schema file should exist on disk: {}",
                path.display()
            );
        }
    }

    #[test]
    fn help_quickstart_and_rule_detail_endpoints_use_live_robot_command_paths() {
        let reg = SchemaRegistry::canonical();

        let help = reg.get("help").expect("help endpoint exists");
        assert_eq!(help.robot_command.as_deref(), Some("robot help"));

        let quickstart = reg.get("quickstart").expect("quickstart endpoint exists");
        assert_eq!(
            quickstart.robot_command.as_deref(),
            Some("robot quick-start")
        );

        let rules_show = reg.get("rules_show").expect("rules_show endpoint exists");
        assert_eq!(
            rules_show.robot_command.as_deref(),
            Some("robot rules show")
        );
    }

    #[test]
    fn reservation_endpoints_use_nested_robot_command_paths() {
        let reg = SchemaRegistry::canonical();

        let reservations = reg
            .get("reservations_list")
            .expect("reservations_list endpoint exists");
        assert_eq!(
            reservations.robot_command.as_deref(),
            Some("robot reservations list")
        );

        let reserve = reg.get("reserve").expect("reserve endpoint exists");
        assert_eq!(
            reserve.robot_command.as_deref(),
            Some("robot reservations reserve")
        );

        let release = reg.get("release").expect("release endpoint exists");
        assert_eq!(
            release.robot_command.as_deref(),
            Some("robot reservations release")
        );
    }

    #[test]
    fn search_diagnostics_endpoints_use_live_robot_command_paths() {
        let reg = SchemaRegistry::canonical();

        let search_explain = reg
            .get("search_explain")
            .expect("search_explain endpoint exists");
        assert_eq!(
            search_explain.robot_command.as_deref(),
            Some("robot search-explain")
        );

        let search_index_stats = reg
            .get("search_index_stats")
            .expect("search_index_stats endpoint exists");
        assert_eq!(
            search_index_stats.robot_command.as_deref(),
            Some("robot search-index stats")
        );

        let search_index_reindex = reg
            .get("search_index_reindex")
            .expect("search_index_reindex endpoint exists");
        assert_eq!(
            search_index_reindex.robot_command.as_deref(),
            Some("robot search-index reindex")
        );
    }

    #[test]
    fn agents_endpoints_use_nested_robot_command_paths() {
        let reg = SchemaRegistry::canonical();

        let agents_list = reg.get("agents_list").expect("agents_list endpoint exists");
        assert_eq!(
            agents_list.robot_command.as_deref(),
            Some("robot agents list")
        );

        let agents_running = reg
            .get("agents_running")
            .expect("agents_running endpoint exists");
        assert_eq!(
            agents_running.robot_command.as_deref(),
            Some("robot agents running")
        );

        let agents_detect = reg
            .get("agents_detect")
            .expect("agents_detect endpoint exists");
        assert_eq!(
            agents_detect.robot_command.as_deref(),
            Some("robot agents detect")
        );
    }

    // --- ApiVersion parse edge cases ---

    #[test]
    fn parse_large_version_numbers() {
        let v = ApiVersion::parse("999.888.777").unwrap();
        assert_eq!(v.major, 999);
        assert_eq!(v.minor, 888);
        assert_eq!(v.patch, 777);
    }

    #[test]
    fn parse_zero_version() {
        let v = ApiVersion::parse("0.0.0").unwrap();
        assert_eq!(v.major, 0);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn parse_extra_parts_still_works() {
        // "1.2.3.4" — has 4 parts, but parse only needs first 3
        let v = ApiVersion::parse("1.2.3.4").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
    }

    #[test]
    fn parse_negative_rejected() {
        // Negative numbers won't parse as u32
        assert!(ApiVersion::parse("-1.0.0").is_none());
    }

    #[test]
    fn parse_non_numeric_rejected() {
        assert!(ApiVersion::parse("a.b.c").is_none());
    }

    #[test]
    fn version_display_roundtrip() {
        let v = ApiVersion {
            major: 5,
            minor: 10,
            patch: 15,
        };
        let s = v.to_string();
        let reparsed = ApiVersion::parse(&s).unwrap();
        assert_eq!(v, reparsed);
    }

    #[test]
    fn version_clone() {
        let v = ApiVersion {
            major: 1,
            minor: 2,
            patch: 3,
        };
        let v2 = v.clone();
        assert_eq!(v, v2);
    }

    #[test]
    fn version_debug() {
        let v = ApiVersion {
            major: 0,
            minor: 1,
            patch: 0,
        };
        let dbg = format!("{:?}", v);
        assert!(dbg.contains("ApiVersion"));
    }

    // --- VersionCompatibility ---

    #[test]
    fn version_compatibility_debug() {
        let vc = VersionCompatibility::Exact;
        let dbg = format!("{:?}", vc);
        assert!(dbg.contains("Exact"));
    }

    #[test]
    fn version_compatibility_copy() {
        let vc = VersionCompatibility::NewerMinor;
        let vc2 = vc;
        assert_eq!(vc, vc2);
    }

    #[test]
    fn version_compatibility_all_variants_distinct() {
        let variants = [
            VersionCompatibility::Exact,
            VersionCompatibility::Compatible,
            VersionCompatibility::NewerMinor,
            VersionCompatibility::Incompatible,
        ];
        for i in 0..variants.len() {
            for j in (i + 1)..variants.len() {
                assert_ne!(variants[i], variants[j]);
            }
        }
    }

    // --- Pre/Post-1.0 symmetry ---

    #[test]
    fn pre1_compatibility_is_symmetric_for_exact() {
        let a = ApiVersion {
            major: 0,
            minor: 3,
            patch: 5,
        };
        assert_eq!(a.compatibility(&a), VersionCompatibility::Exact);
    }

    #[test]
    fn post1_same_patch_different_direction() {
        let reader = ApiVersion {
            major: 2,
            minor: 5,
            patch: 0,
        };
        let wire_older = ApiVersion {
            major: 2,
            minor: 3,
            patch: 0,
        };
        let wire_newer = ApiVersion {
            major: 2,
            minor: 7,
            patch: 0,
        };
        assert_eq!(
            reader.compatibility(&wire_older),
            VersionCompatibility::Compatible
        );
        assert_eq!(
            reader.compatibility(&wire_newer),
            VersionCompatibility::NewerMinor
        );
    }

    // --- EndpointMeta ---

    #[test]
    fn endpoint_meta_serde_roundtrip() {
        let ep = EndpointMeta {
            id: "test".into(),
            title: "Test".into(),
            description: "A test endpoint".into(),
            robot_command: Some("robot test".into()),
            mcp_tool: None,
            schema_file: "wa-robot-test.json".into(),
            stable: false,
            since: "0.2.0".into(),
        };
        let json = serde_json::to_string(&ep).unwrap();
        let parsed: EndpointMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test");
        assert!(!parsed.stable);
        assert!(parsed.mcp_tool.is_none());
    }

    #[test]
    fn endpoint_meta_clone() {
        let ep = EndpointMeta {
            id: "foo".into(),
            title: "Foo".into(),
            description: "bar".into(),
            robot_command: None,
            mcp_tool: Some("wa.foo".into()),
            schema_file: "wa-robot-foo.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let ep2 = ep.clone();
        assert_eq!(ep2.id, ep.id);
        assert_eq!(ep2.mcp_tool, ep.mcp_tool);
    }

    #[test]
    fn endpoint_meta_debug() {
        let ep = EndpointMeta {
            id: "dbg".into(),
            title: "Debug".into(),
            description: "d".into(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "wa-robot-dbg.json".into(),
            stable: true,
            since: "0.1.0".into(),
        };
        let dbg = format!("{:?}", ep);
        assert!(dbg.contains("EndpointMeta"));
    }

    // --- SchemaRegistry ---

    #[test]
    fn schema_registry_default() {
        let reg = SchemaRegistry::default();
        assert!(reg.endpoints.is_empty());
        assert!(reg.version.is_empty());
    }

    #[test]
    fn schema_registry_clone() {
        let reg = SchemaRegistry::canonical();
        let reg2 = reg.clone();
        assert_eq!(reg2.endpoints.len(), reg.endpoints.len());
    }

    #[test]
    fn canonical_all_have_schema_file() {
        let reg = SchemaRegistry::canonical();
        for ep in &reg.endpoints {
            assert!(
                !ep.schema_file.is_empty(),
                "endpoint {} has empty schema_file",
                ep.id
            );
        }
    }

    #[test]
    fn canonical_all_since_valid_semver() {
        let reg = SchemaRegistry::canonical();
        for ep in &reg.endpoints {
            assert!(
                ApiVersion::parse(&ep.since).is_some(),
                "endpoint {} has invalid since: {}",
                ep.id,
                ep.since
            );
        }
    }

    #[test]
    fn canonical_dual_and_robot_only_partition() {
        let reg = SchemaRegistry::canonical();
        let dual_count = reg.dual_surface().count();
        let robot_only_count = reg.robot_only().count();
        // Every endpoint with a robot_command is either dual or robot-only
        let with_robot = reg
            .endpoints
            .iter()
            .filter(|e| e.robot_command.is_some())
            .count();
        assert_eq!(dual_count + robot_only_count, with_robot);
    }

    #[test]
    fn schema_files_are_sorted_and_deduped() {
        let reg = SchemaRegistry::canonical();
        let files = reg.schema_files();
        let mut sorted = files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(files, sorted);
    }

    // --- ChangeKind ---

    #[test]
    fn change_kind_serde_roundtrip_all_variants() {
        let variants = [
            ChangeKind::Added,
            ChangeKind::Removed,
            ChangeKind::RequiredFieldAdded,
            ChangeKind::OptionalFieldAdded,
            ChangeKind::FieldRemoved,
            ChangeKind::TypeChanged,
            ChangeKind::Cosmetic,
        ];
        for kind in &variants {
            let json = serde_json::to_string(kind).unwrap();
            let parsed: ChangeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, kind);
        }
    }

    #[test]
    fn change_kind_copy() {
        let k = ChangeKind::Removed;
        let k2 = k;
        assert_eq!(k, k2);
    }

    #[test]
    fn change_kind_debug() {
        let k = ChangeKind::TypeChanged;
        let dbg = format!("{:?}", k);
        assert!(dbg.contains("TypeChanged"));
    }

    // --- SchemaChange ---

    #[test]
    fn schema_change_clone() {
        let sc = SchemaChange {
            schema_file: "test.json".into(),
            kind: ChangeKind::Added,
            description: "new".into(),
        };
        let sc2 = sc.clone();
        assert_eq!(sc, sc2);
    }

    #[test]
    fn schema_change_serde_roundtrip() {
        let sc = SchemaChange {
            schema_file: "wa-robot-state.json".into(),
            kind: ChangeKind::FieldRemoved,
            description: "removed field X".into(),
        };
        let json = serde_json::to_string(&sc).unwrap();
        let parsed: SchemaChange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sc);
    }

    // --- SchemaDiffResult ---

    #[test]
    fn schema_diff_default() {
        let d = SchemaDiffResult::default();
        assert!(d.from_version.is_empty());
        assert!(d.to_version.is_empty());
        assert!(d.changes.is_empty());
        assert!(!d.has_breaking_changes());
    }

    #[test]
    fn schema_diff_clone() {
        let d = SchemaDiffResult {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            changes: vec![SchemaChange {
                schema_file: "test.json".into(),
                kind: ChangeKind::Cosmetic,
                description: "desc update".into(),
            }],
        };
        let d2 = d.clone();
        assert_eq!(d2.changes.len(), 1);
    }

    #[test]
    fn schema_diff_breaking_changes_iterator() {
        let d = SchemaDiffResult {
            from_version: "1.0.0".into(),
            to_version: "2.0.0".into(),
            changes: vec![
                SchemaChange {
                    schema_file: "a.json".into(),
                    kind: ChangeKind::Added,
                    description: "added".into(),
                },
                SchemaChange {
                    schema_file: "b.json".into(),
                    kind: ChangeKind::Removed,
                    description: "removed".into(),
                },
                SchemaChange {
                    schema_file: "c.json".into(),
                    kind: ChangeKind::Cosmetic,
                    description: "cosmetic".into(),
                },
                SchemaChange {
                    schema_file: "d.json".into(),
                    kind: ChangeKind::TypeChanged,
                    description: "type changed".into(),
                },
            ],
        };
        assert!(d.has_breaking_changes());
        let breaking: Vec<_> = d.breaking_changes().collect();
        assert_eq!(breaking.len(), 2);
        assert_eq!(breaking[0].kind, ChangeKind::Removed);
        assert_eq!(breaking[1].kind, ChangeKind::TypeChanged);
    }

    #[test]
    fn schema_diff_no_breaking_all_compatible() {
        let d = SchemaDiffResult {
            from_version: "0.1.0".into(),
            to_version: "0.1.1".into(),
            changes: vec![
                SchemaChange {
                    schema_file: "a.json".into(),
                    kind: ChangeKind::Added,
                    description: "added".into(),
                },
                SchemaChange {
                    schema_file: "b.json".into(),
                    kind: ChangeKind::OptionalFieldAdded,
                    description: "optional".into(),
                },
                SchemaChange {
                    schema_file: "c.json".into(),
                    kind: ChangeKind::Cosmetic,
                    description: "cosmetic".into(),
                },
            ],
        };
        assert!(!d.has_breaking_changes());
        assert_eq!(d.breaking_changes().count(), 0);
    }

    #[test]
    fn uncovered_schemas_empty_disk() {
        let reg = SchemaRegistry::canonical();
        let uncovered = reg.uncovered_schemas(&[]);
        assert!(uncovered.is_empty());
    }
}
