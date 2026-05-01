//! Schema-driven contract infrastructure for robot command families
//! ([BR-RC-ROBOT-CONTRACT.0] / `ft-hac7w.1`).
//!
//! Foundation for the 5 family closures (`profile`, `checkpoint`,
//! `context`, `work`, `fleet`). One [`FamilyContract`] declaration
//! emits four artifacts:
//!
//! 1. **Proptest input strategies** — each request field carries a
//!    [`ProptestStrategyHint`] consumed by the conformance harness in
//!    `tests/robot_family_conformance/` to build a
//!    `BoxedStrategy<serde_json::Value>`.
//! 2. **JSON Schema** — [`FamilyContract::json_schema`] returns a
//!    Draft 2020-12 schema validatable by the same `jsonschema` runtime
//!    validator already wired into `tests/conformance_robot_envelope_schema.rs`.
//! 3. **MCP tool registration metadata** —
//!    [`FamilyContract::mcp_tool_descriptors`] returns one
//!    [`McpToolDescriptor`] per action, ready for the `fastmcp` seam in
//!    `mcp_framework.rs` to register with a real handler.
//! 4. **Conformance-harness invariants** —
//!    [`FamilyContract::invariants`] enumerates every named
//!    [`ContractInvariant`] across all actions; the harness selects an
//!    implementation by [`InvariantKind`].
//!
//! See `docs/robot-contracts/meta-schema.md` for the prose specification.
//!
//! This module is deliberately dependency-free beyond `serde` /
//! `serde_json` so it is consumable from both the lib (where `proptest`
//! is dev-only) and the conformance harness. The harness adds the
//! `proptest`-specific glue.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Top-level contract
// ============================================================================

/// A complete contract for one robot command family.
///
/// See module docs for the four artifacts emitted from a single value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyContract {
    /// Family name as used in `ft robot <family>` (e.g. `"profile"`).
    pub family_name: String,
    /// One-sentence description used in MCP descriptors and generated docs.
    pub description: String,
    /// Concurrency model for the family as a whole.
    pub concurrency: ConcurrencyModel,
    /// One contract per `(family, action)` pair.
    pub actions: Vec<ActionContract>,
}

impl FamilyContract {
    /// Total number of actions across the family.
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }

    /// Find an action by name.
    #[must_use]
    pub fn action(&self, name: &str) -> Option<&ActionContract> {
        self.actions.iter().find(|a| a.action == name)
    }

    /// Emit a Draft 2020-12 JSON Schema document covering every
    /// request envelope.
    ///
    /// The envelope is discriminated by the literal value of the
    /// top-level `action` field: each branch of the `oneOf` constrains
    /// `action` to a `const` and `params` to that action's request
    /// schema. This avoids the "multiple branches match" ambiguity
    /// that arises when two actions happen to share field names.
    ///
    /// This is the schema the IPC boundary validates against, and the
    /// shape downstream client-codegen consumes.
    #[must_use]
    pub fn json_schema(&self) -> serde_json::Value {
        let one_of: Vec<serde_json::Value> = self
            .actions
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": "object",
                    "required": ["action", "params"],
                    "properties": {
                        "action": { "const": a.action.clone() },
                        "params": a.request_schema_value(),
                    },
                    "additionalProperties": false,
                })
            })
            .collect();
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": format!("https://frankenterm.dev/schema/robot/family/{}.json", self.family_name),
            "title": format!("ft robot {} request envelope", self.family_name),
            "description": self.description,
            "oneOf": one_of,
        })
    }

    /// One MCP tool descriptor per action. The `fastmcp` server seam
    /// in `mcp_framework.rs` can register the whole family with one
    /// `for descriptor in family.mcp_tool_descriptors() { server.tool(...) }`
    /// loop.
    #[must_use]
    pub fn mcp_tool_descriptors(&self) -> Vec<McpToolDescriptor> {
        self.actions
            .iter()
            .map(|a| a.mcp_tool_descriptor())
            .collect()
    }

    /// Flat enumeration of every invariant across all actions.
    ///
    /// Each tuple is `(action_name, invariant)` so the harness can
    /// produce a stable test name (`<family>__<action>__<invariant>`).
    #[must_use]
    pub fn invariants(&self) -> Vec<(&str, &ContractInvariant)> {
        let mut out = Vec::new();
        for a in &self.actions {
            for inv in &a.invariants {
                out.push((a.action.as_str(), inv));
            }
        }
        out
    }

    /// One [`ProptestSeed`] per action — the harness consumes this to
    /// build request strategies.
    #[must_use]
    pub fn proptest_seeds(&self) -> Vec<ProptestSeed> {
        self.actions
            .iter()
            .map(|a| ProptestSeed {
                action: a.action.clone(),
                fields: a.request_proptest.clone(),
            })
            .collect()
    }

    /// Validate the contract for internal consistency. Returns a list
    /// of violations as human-readable strings (empty Vec = clean).
    ///
    /// Used by the conformance harness as a meta-test: a malformed
    /// contract should never reach the per-family checks.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.family_name.is_empty() {
            errs.push("family_name is empty".to_string());
        }
        if self.actions.is_empty() {
            errs.push(format!("family {} declares no actions", self.family_name));
        }
        let mut seen_actions: BTreeMap<&str, usize> = BTreeMap::new();
        let mut seen_mcp_names: BTreeMap<&str, usize> = BTreeMap::new();
        for action in &self.actions {
            *seen_actions.entry(action.action.as_str()).or_insert(0) += 1;
            *seen_mcp_names
                .entry(action.mcp_tool_name.as_str())
                .or_insert(0) += 1;

            let kinds: Vec<InvariantKind> =
                action.invariants.iter().map(|i| i.kind.clone()).collect();
            if !kinds
                .iter()
                .any(|k| matches!(k, InvariantKind::Determinism))
            {
                errs.push(format!(
                    "{}.{}: missing required Determinism invariant",
                    self.family_name, action.action
                ));
            }
            if !kinds
                .iter()
                .any(|k| matches!(k, InvariantKind::ResponseShape))
            {
                errs.push(format!(
                    "{}.{}: missing required ResponseShape invariant",
                    self.family_name, action.action
                ));
            }
            // Mutating actions with strict failure semantics must
            // declare AtomicOnFailure (or upgrade to a Custom check).
            let mutating = !matches!(
                action.side_effects,
                SideEffectSurface {
                    ref events_emitted,
                    ref storage_tables_mutated,
                    ref ipc_targets,
                } if events_emitted.is_empty()
                    && storage_tables_mutated.is_empty()
                    && ipc_targets.is_empty()
            );
            if mutating
                && matches!(
                    action.failure_semantics,
                    FailureSemantics::MustNotPartiallyMutate
                )
                && !kinds
                    .iter()
                    .any(|k| matches!(k, InvariantKind::AtomicOnFailure))
            {
                errs.push(format!(
                    "{}.{}: mutating + MustNotPartiallyMutate requires \
                     AtomicOnFailure invariant",
                    self.family_name, action.action
                ));
            }
        }
        for (name, count) in &seen_actions {
            if *count > 1 {
                errs.push(format!(
                    "duplicate action name `{name}` in family {}",
                    self.family_name
                ));
            }
        }
        for (name, count) in &seen_mcp_names {
            if *count > 1 {
                errs.push(format!(
                    "duplicate mcp_tool_name `{name}` in family {}",
                    self.family_name
                ));
            }
        }
        errs
    }
}

// ============================================================================
// Per-action contract
// ============================================================================

/// One action within a family — e.g. `profile show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionContract {
    /// Action name as used in `ft robot <family> <action>`.
    pub action: String,
    /// Full robot CLI form for documentation (`"robot profile show"`).
    pub robot_command: String,
    /// MCP tool name (`"ft.profile.show"`); MUST be unique per process.
    pub mcp_tool_name: String,
    /// One-sentence description.
    pub description: String,
    /// Idempotency class — see [`IdempotencyClass`].
    pub idempotency: IdempotencyClass,
    /// Failure semantics — see [`FailureSemantics`].
    pub failure_semantics: FailureSemantics,
    /// Observable side-effect surface declared by the action.
    pub side_effects: SideEffectSurface,
    /// Schema shape for the request body.
    pub request_schema: SchemaShape,
    /// Schema shape for the response `data` payload.
    pub response_schema: SchemaShape,
    /// One [`ProptestField`] per request field — drives input fuzzing.
    pub request_proptest: Vec<ProptestField>,
    /// Named conformance invariants the harness enumerates.
    pub invariants: Vec<ContractInvariant>,
}

impl ActionContract {
    /// JSON Schema fragment for this action's request body.
    fn request_schema_value(&self) -> serde_json::Value {
        let mut schema = self.request_schema.to_json_schema();
        // Tag the schema with the discriminator so `oneOf` matching is
        // unambiguous in the family-level envelope.
        if let Some(obj) = schema.as_object_mut() {
            obj.insert(
                "title".to_string(),
                serde_json::Value::String(format!("{} request", self.action)),
            );
            obj.insert(
                "$comment".to_string(),
                serde_json::Value::String(format!("action={}", self.action)),
            );
        }
        schema
    }

    /// Build the MCP tool descriptor for this action.
    fn mcp_tool_descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: self.mcp_tool_name.clone(),
            description: self.description.clone(),
            input_schema: self.request_schema.to_json_schema(),
            idempotency: self.idempotency.clone(),
        }
    }
}

// ============================================================================
// Idempotency / failure / concurrency / side-effects
// ============================================================================

/// Idempotency class for an action. See `meta-schema.md` §3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyClass {
    /// Two consecutive calls produce no extra side effects.
    Idempotent,
    /// Distinct requests can be reordered without observable change.
    Commutative,
    /// Requests must be serialized; reordering changes the result.
    Sequential,
}

/// Failure semantics for a mutating action. See `meta-schema.md` §4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSemantics {
    /// A failed request leaves storage / IPC / event state untouched.
    MustNotPartiallyMutate,
    /// Partial mutation is allowed iff the failure response carries a
    /// typed receipt naming what landed.
    CanPartiallyMutateWithReceipt,
    /// No durable effect — failure visible only via the response.
    FireAndForget,
}

/// Concurrency model for a family. See `meta-schema.md` §5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyModel {
    /// At most one concurrent invocation of this family.
    Serializable,
    /// At most one per pane.
    PerPaneSerial,
    /// Multiple invocations may run concurrently with no interlock.
    Parallel,
}

/// Observable side-effect surface declared by an action. See §6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SideEffectSurface {
    /// Event-bus event types the action MAY emit.
    #[serde(default)]
    pub events_emitted: Vec<String>,
    /// Storage tables the action MAY mutate.
    #[serde(default)]
    pub storage_tables_mutated: Vec<String>,
    /// IPC destinations.
    #[serde(default)]
    pub ipc_targets: Vec<String>,
}

impl SideEffectSurface {
    /// The empty side-effect surface — read-only actions.
    #[must_use]
    pub fn read_only() -> Self {
        Self::default()
    }

    /// True when the action declares no side effects (read-only).
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.events_emitted.is_empty()
            && self.storage_tables_mutated.is_empty()
            && self.ipc_targets.is_empty()
    }
}

// ============================================================================
// Schema shape
// ============================================================================

/// Stripped-down JSON Schema subset sufficient for request envelopes.
///
/// See `meta-schema.md` §7 for what's deliberately *not* covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaShape {
    /// Outer kind — `Object` for typical request bodies.
    pub kind: SchemaKind,
    /// Fields, when `kind == Object`. Empty otherwise.
    #[serde(default)]
    pub fields: Vec<SchemaField>,
}

impl SchemaShape {
    /// Render as a Draft 2020-12 JSON Schema fragment.
    #[must_use]
    pub fn to_json_schema(&self) -> serde_json::Value {
        match self.kind {
            SchemaKind::Object => {
                let mut props = serde_json::Map::new();
                let mut required = Vec::new();
                for field in &self.fields {
                    let mut prop = field.kind.to_json_schema_atom();
                    if let (Some(obj), Some(desc)) =
                        (prop.as_object_mut(), field.description.as_ref())
                    {
                        obj.insert(
                            "description".to_string(),
                            serde_json::Value::String(desc.clone()),
                        );
                    }
                    props.insert(field.name.clone(), prop);
                    if field.required {
                        required.push(serde_json::Value::String(field.name.clone()));
                    }
                }
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String("object".to_string()),
                );
                obj.insert("properties".to_string(), serde_json::Value::Object(props));
                if !required.is_empty() {
                    obj.insert("required".to_string(), serde_json::Value::Array(required));
                }
                obj.insert(
                    "additionalProperties".to_string(),
                    serde_json::Value::Bool(false),
                );
                serde_json::Value::Object(obj)
            }
            _ => self.kind.to_json_schema_atom(),
        }
    }
}

/// JSON Schema primitive kinds the meta-schema supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    Object,
    Array,
    String,
    Integer,
    Boolean,
    Null,
}

impl SchemaKind {
    fn to_json_schema_atom(self) -> serde_json::Value {
        let ty = match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Null => "null",
        };
        serde_json::json!({ "type": ty })
    }
}

/// One field of an object-kind [`SchemaShape`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaField {
    /// Field name.
    pub name: String,
    /// Field kind.
    pub kind: SchemaKind,
    /// Whether the field is required.
    pub required: bool,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
}

// ============================================================================
// Proptest seeds
// ============================================================================

/// Per-action seed describing how the harness should generate inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProptestSeed {
    /// Action name.
    pub action: String,
    /// Per-field strategy hints.
    pub fields: Vec<ProptestField>,
}

/// One field of a request body, plus its strategy hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProptestField {
    /// Field name (must match a [`SchemaField::name`] in the same action).
    pub name: String,
    /// Strategy hint — see [`ProptestStrategyHint`].
    pub strategy: ProptestStrategyHint,
}

/// What kind of input the harness should generate for a request field.
///
/// New variants are added in lockstep with the harness's hint→strategy
/// translator in `tests/robot_family_conformance/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProptestStrategyHint {
    /// Printable ASCII string up to `max_len` chars.
    AsciiString { max_len: usize },
    /// Integer in `[min, max]`.
    U32Range { min: u32, max: u32 },
    /// Boolean.
    Bool,
    /// `Some(ascii)` / `None`.
    OptionString { max_len: usize },
    /// `HashMap<String, String>` with up to `max_entries` keys.
    StringMap { max_entries: usize },
}

// ============================================================================
// Conformance invariants
// ============================================================================

/// One named conformance check the harness runs against a real handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractInvariant {
    /// Stable identifier (e.g. `"show_is_idempotent"`).
    pub name: String,
    /// What kind of check the harness runs.
    pub kind: InvariantKind,
    /// Human-readable rationale.
    pub description: String,
}

/// What flavor of conformance check to run. See `meta-schema.md` §9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvariantKind {
    /// Same input → same output across two runs.
    Determinism,
    /// Repeating the request twice produces no extra side effects.
    Idempotence,
    /// Failed mid-flight requests must not leave partial state.
    AtomicOnFailure,
    /// Two distinct successful requests are order-independent.
    Commutativity,
    /// Response `data` validates against `response_schema`.
    ResponseShape,
    /// Family-specific predicate; the family's conformance test file
    /// supplies the implementation, keyed by `name`.
    Custom { name: String },
}

// ============================================================================
// MCP descriptor
// ============================================================================

/// Metadata the `mcp_framework.rs` seam needs to register an action as
/// a `fastmcp` tool. The handler itself is supplied separately by the
/// family implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    /// Tool name (`"ft.profile.show"`).
    pub name: String,
    /// Tool description (one sentence).
    pub description: String,
    /// JSON Schema for the input.
    pub input_schema: serde_json::Value,
    /// Idempotency hint exposed to the MCP client.
    pub idempotency: IdempotencyClass,
}

// ============================================================================
// Profile family — proof-of-concept
// ============================================================================

/// Schema-DSL declaration for the `profile` family. Proves the
/// foundation: `profile_family_contract().validate()` is empty,
/// the JSON Schema validates against Draft 2020-12, and every
/// invariant kind has a harness implementation.
///
/// The actual handler implementations live elsewhere (this contract
/// is consumed by both the harness and the future MCP wiring); request
/// / response shapes mirror the existing types in
/// [`crate::robot_ntm_surface`].
#[must_use]
pub fn profile_family_contract() -> FamilyContract {
    FamilyContract {
        family_name: "profile".to_string(),
        description: "Session profile management — list, show, apply, validate.".to_string(),
        concurrency: ConcurrencyModel::Serializable,
        actions: vec![
            // ------------------------------------------------------------------
            // profile list
            // ------------------------------------------------------------------
            ActionContract {
                action: "list".to_string(),
                robot_command: "robot profile list".to_string(),
                mcp_tool_name: "ft.profile.list".to_string(),
                description: "List available session profiles.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "role_filter".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some("Restrict to a single role.".to_string()),
                        },
                        SchemaField {
                            name: "tag_filter".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Restrict to profiles carrying this tag.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "profiles".to_string(),
                        kind: SchemaKind::Array,
                        required: true,
                        description: Some("Profile summaries.".to_string()),
                    }],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "role_filter".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                    ProptestField {
                        name: "tag_filter".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "list_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same filter inputs return identical profile lists."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "list_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against `profiles[]` schema."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // profile show
            // ------------------------------------------------------------------
            ActionContract {
                action: "show".to_string(),
                robot_command: "robot profile show".to_string(),
                mcp_tool_name: "ft.profile.show".to_string(),
                description: "Show details of a single profile by name.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "name".to_string(),
                        kind: SchemaKind::String,
                        required: true,
                        description: Some("Profile name.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: None,
                        },
                        SchemaField {
                            name: "role".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: None,
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "name".to_string(),
                    strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "show_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Two consecutive show calls return identical data."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "show_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against `ProfileShowData` shape."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // profile apply
            // ------------------------------------------------------------------
            ActionContract {
                action: "apply".to_string(),
                robot_command: "robot profile apply".to_string(),
                mcp_tool_name: "ft.profile.apply".to_string(),
                description: "Spawn or configure panes from a profile (mutating, transactional)."
                    .to_string(),
                idempotency: IdempotencyClass::Sequential,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["profile.applied".to_string()],
                    storage_tables_mutated: vec!["profiles_applied_log".to_string()],
                    ipc_targets: vec!["mux".to_string()],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Profile name to apply.".to_string()),
                        },
                        SchemaField {
                            name: "count".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Number of panes to spawn.".to_string()),
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: false,
                            description: Some("Preview only — no mutation.".to_string()),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "profile_name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: None,
                        },
                        SchemaField {
                            name: "panes_spawned".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: None,
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: None,
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "name".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "count".to_string(),
                        strategy: ProptestStrategyHint::U32Range { min: 1, max: 16 },
                    },
                    ProptestField {
                        name: "dry_run".to_string(),
                        strategy: ProptestStrategyHint::Bool,
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "apply_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description:
                            "Same (name, count, dry_run) produces identical observable outcome."
                                .to_string(),
                    },
                    ContractInvariant {
                        name: "apply_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against `ProfileApplyData` shape."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "apply_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description:
                            "A failed apply leaves no panes spawned and no log row written."
                                .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // profile validate
            // ------------------------------------------------------------------
            ActionContract {
                action: "validate".to_string(),
                robot_command: "robot profile validate".to_string(),
                mcp_tool_name: "ft.profile.validate".to_string(),
                description: "Validate a profile definition without applying it.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "name".to_string(),
                        kind: SchemaKind::String,
                        required: true,
                        description: Some("Profile name to validate.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: None,
                        },
                        SchemaField {
                            name: "valid".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: None,
                        },
                        SchemaField {
                            name: "issues".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: None,
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "name".to_string(),
                    strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "validate_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same name produces identical (valid, issues) tuple."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "validate_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against `ProfileValidateData` shape."
                            .to_string(),
                    },
                ],
            },
        ],
    }
}

// ============================================================================
// Checkpoint family — ft-hac7w.3 / BR-RC-ROBOT-CONTRACT.2
// ============================================================================

/// Schema-DSL declaration for the `checkpoint` family. Wires
/// the `RobotCommands::Checkpoint` family — `save` / `rollback`
/// / `list` — into the schema-driven contract infrastructure.
///
/// Wires into the existing `ft snapshot` + session_restore
/// machinery (no new schema needed). The state-space proof of
/// the save→rollback transition lives in
/// `crate::robot_checkpoint_state_machine`; the conformance
/// harness extension lives in
/// `tests/robot_family_conformance/checkpoint.rs` (or as a
/// sibling test file once the runner is split).
///
/// Contract semantics from the bead:
/// - `save` is **idempotent** — content-addressed snapshot ID;
///   re-issuing with the same source state returns the same id
///   without an extra storage write.
/// - `rollback` requires an approval token (cross-pane
///   mutation); MUST NOT partially mutate.
/// - `list` is a **pure read**.
/// - Concurrency: serializable per session.
#[must_use]
pub fn checkpoint_family_contract() -> FamilyContract {
    FamilyContract {
        family_name: "checkpoint".to_string(),
        description:
            "Session checkpoint management — save (idempotent), rollback (approval-gated), list."
                .to_string(),
        concurrency: ConcurrencyModel::Serializable,
        actions: vec![
            // ------------------------------------------------------------------
            // checkpoint save
            // ------------------------------------------------------------------
            ActionContract {
                action: "save".to_string(),
                robot_command: "robot checkpoint save".to_string(),
                mcp_tool_name: "ft.checkpoint.save".to_string(),
                description: "Persist a session snapshot. Content-addressed; re-issuing with \
                              the same source state returns the same checkpoint id."
                    .to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["checkpoint.saved".to_string()],
                    storage_tables_mutated: vec!["snapshots".to_string()],
                    ipc_targets: vec![],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "session_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Session whose state to snapshot.".to_string()),
                        },
                        SchemaField {
                            name: "label".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Optional human-readable label for the checkpoint.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "metadata".to_string(),
                            kind: SchemaKind::Object,
                            required: false,
                            description: Some(
                                "Optional caller-supplied metadata; opaque to the server."
                                    .to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "checkpoint_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Content-addressed checkpoint id (BLAKE3 hex).".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "session_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Session id.".to_string()),
                        },
                        SchemaField {
                            name: "created_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms.".to_string()),
                        },
                        SchemaField {
                            name: "is_duplicate".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some(
                                "True iff the same content already had a checkpoint id; \
                                 the existing one was returned without a new storage write."
                                    .to_string(),
                            ),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "session_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "label".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                    ProptestField {
                        name: "metadata".to_string(),
                        strategy: ProptestStrategyHint::StringMap { max_entries: 4 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "save_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (session_id, label, metadata) on the same source \
                                      state returns the same checkpoint id."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "save_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the save response schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "save_is_idempotent".to_string(),
                        kind: InvariantKind::Idempotence,
                        description: "Re-issuing save with the same content does not produce \
                                      a second snapshots-table row."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "save_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed save leaves no row in the snapshots table and \
                                      emits no checkpoint.saved event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "save_content_address_collision_resistance".to_string(),
                        kind: InvariantKind::Custom {
                            name: "save_content_address_collision_resistance".to_string(),
                        },
                        description: "Two saves with distinct source-state content produce \
                                      distinct checkpoint ids (BLAKE3 collision resistance)."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // checkpoint rollback
            // ------------------------------------------------------------------
            ActionContract {
                action: "rollback".to_string(),
                robot_command: "robot checkpoint rollback".to_string(),
                mcp_tool_name: "ft.checkpoint.rollback".to_string(),
                description: "Restore a session to a previously-saved checkpoint. \
                              Requires an approval token; cross-pane mutation."
                    .to_string(),
                idempotency: IdempotencyClass::Sequential,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["checkpoint.rolled_back".to_string()],
                    storage_tables_mutated: vec![
                        "snapshots".to_string(),
                        "session_state".to_string(),
                    ],
                    ipc_targets: vec!["session_restore".to_string()],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "checkpoint_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Checkpoint id to roll back to (BLAKE3 hex).".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "approval_token".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Approval token authorizing the cross-pane rollback.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: false,
                            description: Some(
                                "If true, validate without mutating session state.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "checkpoint_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Checkpoint id rolled back to.".to_string()),
                        },
                        SchemaField {
                            name: "session_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Session id.".to_string()),
                        },
                        SchemaField {
                            name: "panes_restored".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Number of panes restored.".to_string()),
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some("Whether the request was a dry run.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "checkpoint_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 64 },
                    },
                    ProptestField {
                        name: "approval_token".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "dry_run".to_string(),
                        strategy: ProptestStrategyHint::Bool,
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "rollback_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (checkpoint_id, approval_token, dry_run) against \
                                      the same starting state produces identical observable \
                                      outcome."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rollback_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description:
                            "Response data validates against the rollback response schema."
                                .to_string(),
                    },
                    ContractInvariant {
                        name: "rollback_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed rollback leaves session_state untouched and \
                                      emits no checkpoint.rolled_back event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rollback_requires_approval".to_string(),
                        kind: InvariantKind::Custom {
                            name: "rollback_requires_approval".to_string(),
                        },
                        description:
                            "A rollback request without a valid approval_token returns Denied \
                             with no side effects."
                                .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // checkpoint list
            // ------------------------------------------------------------------
            ActionContract {
                action: "list".to_string(),
                robot_command: "robot checkpoint list".to_string(),
                mcp_tool_name: "ft.checkpoint.list".to_string(),
                description: "List checkpoints for a session.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "session_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Session whose checkpoints to list.".to_string()),
                        },
                        SchemaField {
                            name: "limit".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Cap on entries returned; default 100.".to_string()),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "session_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Session id.".to_string()),
                        },
                        SchemaField {
                            name: "checkpoints".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: Some("Checkpoint summaries newest-first.".to_string()),
                        },
                        SchemaField {
                            name: "truncated".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some("True iff `limit` clipped the result.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "session_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "limit".to_string(),
                        strategy: ProptestStrategyHint::U32Range { min: 1, max: 1000 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "list_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (session_id, limit) on the same store produces the \
                                      same response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "list_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the list response schema."
                            .to_string(),
                    },
                ],
            },
        ],
    }
}

// ============================================================================
// Work family — ft-hac7w.5 / BR-RC-ROBOT-CONTRACT.4
// ============================================================================

/// Schema-DSL declaration for the `work` family —
/// `claim` / `complete` / `release` / `status` / `list`.
/// Bead-style work queue per agent, composing with the `br`
/// ownership model. The Stateright-shape state-space proof
/// lives in [`crate::robot_work_state_machine`].
///
/// Headline contract semantics from the bead:
///
/// - `claim` is **non-idempotent** (returns 409-equivalent
///   `Denied { reason: "already_claimed" }` on existing claim).
/// - `complete` is **idempotent on owned claim** (re-completing
///   the same claim is a no-op; completing a claim owned by
///   another agent is denied).
/// - `release` returns the claim to the queue.
/// - `status` / `list` are pure reads.
///
/// Concurrency: serializable per `claim_id`, parallel across
/// distinct claim ids.
///
/// Stateright invariants the harness verifies:
///
/// 1. **NoDoubleClaim** — no two agents hold the same
///    `claim_id` simultaneously.
/// 2. **NoClaimLeak** — every claim eventually releases (no
///    leak under any failure interleaving).
/// 3. **CompletedIsDurable** — completed work is durable; no
///    lost-completion under crash + restart.
#[must_use]
pub fn work_family_contract() -> FamilyContract {
    FamilyContract {
        family_name: "work".to_string(),
        description: "Bead-style work queue — claim (exclusive), complete, release, status, list."
            .to_string(),
        concurrency: ConcurrencyModel::PerPaneSerial,
        actions: vec![
            // ------------------------------------------------------------------
            // work claim
            // ------------------------------------------------------------------
            ActionContract {
                action: "claim".to_string(),
                robot_command: "robot work claim".to_string(),
                mcp_tool_name: "ft.work.claim".to_string(),
                description: "Claim exclusive ownership of a work item. Non-idempotent: \
                              returns Denied if already claimed by a different agent."
                    .to_string(),
                idempotency: IdempotencyClass::Sequential,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["work.claimed".to_string()],
                    storage_tables_mutated: vec!["work_claims".to_string()],
                    ipc_targets: vec![],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Stable claim id (compatible with `br` ownership scheme)."
                                    .to_string(),
                            ),
                        },
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Agent acquiring the claim.".to_string()),
                        },
                        SchemaField {
                            name: "ttl_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some(
                                "Optional auto-release deadline in epoch ms.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "claimed_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms when the claim succeeded.".to_string()),
                        },
                        SchemaField {
                            name: "expires_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Epoch ms when the claim auto-releases.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "claim_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "agent_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "ttl_ms".to_string(),
                        strategy: ProptestStrategyHint::U32Range {
                            min: 0,
                            max: 3_600_000,
                        },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "claim_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (claim_id, agent_id, ttl_ms) on the same store \
                                      produces the same outcome."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "claim_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the claim response schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "claim_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed claim leaves no row in work_claims and emits no \
                                      work.claimed event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "no_double_claim".to_string(),
                        kind: InvariantKind::Custom {
                            name: "no_double_claim".to_string(),
                        },
                        description: "Two distinct agents cannot hold the same claim_id \
                                      simultaneously. Verified at the state-machine level."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // work complete
            // ------------------------------------------------------------------
            ActionContract {
                action: "complete".to_string(),
                robot_command: "robot work complete".to_string(),
                mcp_tool_name: "ft.work.complete".to_string(),
                description: "Mark an owned claim as completed. Idempotent on the owning \
                              agent."
                    .to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["work.completed".to_string()],
                    storage_tables_mutated: vec!["work_claims".to_string()],
                    ipc_targets: vec![],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Claim to complete.".to_string()),
                        },
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Agent owning the claim.".to_string()),
                        },
                        SchemaField {
                            name: "result".to_string(),
                            kind: SchemaKind::Object,
                            required: false,
                            description: Some(
                                "Optional caller-supplied completion payload.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "completed_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms when completion landed.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "claim_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "agent_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "result".to_string(),
                        strategy: ProptestStrategyHint::StringMap { max_entries: 4 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "complete_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (claim_id, agent_id) on the same state produces \
                                      identical observable outcome."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "complete_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the complete response \
                                      schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "complete_is_idempotent".to_string(),
                        kind: InvariantKind::Idempotence,
                        description: "Re-completing the same owned claim does not produce \
                                      a second work.completed event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "complete_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed complete leaves the work_claims row in its \
                                      prior state and emits no event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "completed_is_durable".to_string(),
                        kind: InvariantKind::Custom {
                            name: "completed_is_durable".to_string(),
                        },
                        description: "Once a claim is marked Completed, no transition removes \
                                      that completion. Verified at the state-machine level \
                                      under crash + restart simulation."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // work release
            // ------------------------------------------------------------------
            ActionContract {
                action: "release".to_string(),
                robot_command: "robot work release".to_string(),
                mcp_tool_name: "ft.work.release".to_string(),
                description: "Release a claim back to the queue without marking it \
                              completed."
                    .to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["work.released".to_string()],
                    storage_tables_mutated: vec!["work_claims".to_string()],
                    ipc_targets: vec![],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Claim to release.".to_string()),
                        },
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Agent owning the claim.".to_string()),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "released_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms when the release landed.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "claim_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "agent_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "release_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (claim_id, agent_id) on the same state produces \
                                      identical observable outcome."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "release_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the release response \
                                      schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "release_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed release leaves the work_claims row in its \
                                      prior state and emits no event."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // work status
            // ------------------------------------------------------------------
            ActionContract {
                action: "status".to_string(),
                robot_command: "robot work status".to_string(),
                mcp_tool_name: "ft.work.status".to_string(),
                description: "Look up the status of a single claim.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "claim_id".to_string(),
                        kind: SchemaKind::String,
                        required: true,
                        description: Some("Claim to look up.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claim_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "state".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "One of `unclaimed` / `claimed` / `completed`.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "owner".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some("Owning agent (when claimed/completed).".to_string()),
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "claim_id".to_string(),
                    strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "status_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same claim_id on the same store produces the same \
                                      response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "status_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the status response \
                                      schema."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // work list
            // ------------------------------------------------------------------
            ActionContract {
                action: "list".to_string(),
                robot_command: "robot work list".to_string(),
                mcp_tool_name: "ft.work.list".to_string(),
                description: "List claims, optionally filtered by agent or state.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "agent_id".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Restrict to claims owned by this agent.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "state_filter".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Restrict to one of `unclaimed`/`claimed`/`completed`.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "limit".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Cap on entries returned; default 100.".to_string()),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "claims".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: Some("Claim summaries.".to_string()),
                        },
                        SchemaField {
                            name: "truncated".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some("True iff `limit` clipped the result.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "agent_id".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                    ProptestField {
                        name: "state_filter".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 16 },
                    },
                    ProptestField {
                        name: "limit".to_string(),
                        strategy: ProptestStrategyHint::U32Range { min: 1, max: 1000 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "list_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same filters on the same store produce the same response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "list_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the list response schema."
                            .to_string(),
                    },
                ],
            },
        ],
    }
}

// ============================================================================
// Fleet family — ft-hac7w.6 / BR-RC-ROBOT-CONTRACT.5
// ============================================================================

/// Schema-DSL declaration for the `fleet` family —
/// `status` / `launch` / `stop` / `describe`. Surfaces the
/// existing `frankenterm-core-fleet` sub-crate through robot
/// mode. Mutating actions (`launch` / `stop`) route through
/// the TX engine for atomicity, cross-linking to the kill-
/// switch state-space proof at
/// [`crate::tx_killswitch_model`] (`ft-x0666.4`).
///
/// Headline contract semantics:
///
/// - `status` / `describe` are pure reads; idempotent.
/// - `launch` is **non-idempotent** — returns existing
///   `fleet_id` if a fleet with the same `name` already exists
///   (conflict signaled with `Denied { reason: "already_running" }`).
/// - `stop` is **idempotent** on a Stopped/RolledBack terminal.
/// - Concurrency: serializable per `fleet_id`, parallel across
///   distinct fleets.
#[must_use]
pub fn fleet_family_contract() -> FamilyContract {
    FamilyContract {
        family_name: "fleet".to_string(),
        description:
            "Fleet management — status, launch (TX-engine-atomic), stop (TX-engine-atomic), \
             describe."
                .to_string(),
        concurrency: ConcurrencyModel::PerPaneSerial,
        actions: vec![
            // ------------------------------------------------------------------
            // fleet status
            // ------------------------------------------------------------------
            ActionContract {
                action: "status".to_string(),
                robot_command: "robot fleet status".to_string(),
                mcp_tool_name: "ft.fleet.status".to_string(),
                description: "Aggregate health snapshot for one or all fleets.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "fleet_id".to_string(),
                        kind: SchemaKind::String,
                        required: false,
                        description: Some("Optional — restrict to a single fleet.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "fleets".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: Some("Per-fleet status records.".to_string()),
                        },
                        SchemaField {
                            name: "queried_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "fleet_id".to_string(),
                    strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "status_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same fleet_id on the same store produces identical \
                                      response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "status_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the status response \
                                      schema."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // fleet launch
            // ------------------------------------------------------------------
            ActionContract {
                action: "launch".to_string(),
                robot_command: "robot fleet launch".to_string(),
                mcp_tool_name: "ft.fleet.launch".to_string(),
                description: "Launch a fleet of panes through the TX engine. \
                              Non-idempotent: returns Denied with existing fleet_id if a \
                              fleet with the same name is already running."
                    .to_string(),
                idempotency: IdempotencyClass::Sequential,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec![
                        "fleet.launching".to_string(),
                        "fleet.launched".to_string(),
                        "fleet.launch_failed".to_string(),
                        "fleet.launch_compensated".to_string(),
                    ],
                    storage_tables_mutated: vec!["fleets".to_string()],
                    ipc_targets: vec!["mux".to_string(), "tx_engine".to_string()],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Fleet name (must be unique across running fleets).".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "pane_count".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Number of panes to spawn.".to_string()),
                        },
                        SchemaField {
                            name: "profile".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Optional profile name to apply to each pane.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: false,
                            description: Some(
                                "If true, run TX prepare phase only; do not commit.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "fleet_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Stable fleet id.".to_string()),
                        },
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "tx_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "TX-engine transaction id covering the launch.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "panes_launched".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Count of panes that committed.".to_string()),
                        },
                        SchemaField {
                            name: "dry_run".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "name".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "pane_count".to_string(),
                        strategy: ProptestStrategyHint::U32Range { min: 1, max: 16 },
                    },
                    ProptestField {
                        name: "profile".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                    ProptestField {
                        name: "dry_run".to_string(),
                        strategy: ProptestStrategyHint::Bool,
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "launch_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (name, pane_count, profile, dry_run) on the same \
                                      store produces identical observable outcome (same tx_id, \
                                      same fleet_id assignment scheme)."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "launch_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the launch response \
                                      schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "launch_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed launch leaves no row in fleets, no panes \
                                      spawned, no fleet.launched event. Compensating \
                                      transactions roll back any prepared rows."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "launch_no_double_running".to_string(),
                        kind: InvariantKind::Custom {
                            name: "launch_no_double_running".to_string(),
                        },
                        description: "Two distinct successful launches cannot share a name \
                                      while both are Running. Verified at the state-machine \
                                      level under the TX engine."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // fleet stop
            // ------------------------------------------------------------------
            ActionContract {
                action: "stop".to_string(),
                robot_command: "robot fleet stop".to_string(),
                mcp_tool_name: "ft.fleet.stop".to_string(),
                description: "Stop a fleet through the TX engine. Idempotent on \
                              already-stopped fleets."
                    .to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec![
                        "fleet.stopping".to_string(),
                        "fleet.stopped".to_string(),
                        "fleet.stop_failed".to_string(),
                    ],
                    storage_tables_mutated: vec!["fleets".to_string()],
                    ipc_targets: vec!["mux".to_string(), "tx_engine".to_string()],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "fleet_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Fleet to stop.".to_string()),
                        },
                        SchemaField {
                            name: "force".to_string(),
                            kind: SchemaKind::Boolean,
                            required: false,
                            description: Some(
                                "If true, kill panes without graceful shutdown.".to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "fleet_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "tx_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("TX-engine transaction id.".to_string()),
                        },
                        SchemaField {
                            name: "panes_stopped".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Count of panes that stopped.".to_string()),
                        },
                        SchemaField {
                            name: "is_duplicate".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some(
                                "True iff the fleet was already Stopped — no-op stop.".to_string(),
                            ),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "fleet_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "force".to_string(),
                        strategy: ProptestStrategyHint::Bool,
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "stop_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (fleet_id, force) against the same starting state \
                                      produces identical observable outcome."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "stop_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the stop response schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "stop_is_idempotent".to_string(),
                        kind: InvariantKind::Idempotence,
                        description: "Re-stopping an already-Stopped fleet returns \
                                      is_duplicate=true with no second fleet.stopped event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "stop_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed stop leaves the fleets row in its prior state. \
                                      TX engine rolls back any prepared transitions."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "stop_completes_under_kill_switch_hardstop".to_string(),
                        kind: InvariantKind::Custom {
                            name: "stop_completes_under_kill_switch_hardstop".to_string(),
                        },
                        description: "Stop drains to a terminal state (Stopped or RolledBack) \
                                      even when MissionKillSwitchLevel is HardStop. \
                                      Cross-link to ft-x0666.4 tx_killswitch_model."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // fleet describe
            // ------------------------------------------------------------------
            ActionContract {
                action: "describe".to_string(),
                robot_command: "robot fleet describe".to_string(),
                mcp_tool_name: "ft.fleet.describe".to_string(),
                description: "Detailed description of a single fleet — pane list, profile, \
                              alerts, runbook references."
                    .to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "fleet_id".to_string(),
                        kind: SchemaKind::String,
                        required: true,
                        description: Some("Fleet to describe.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "fleet_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "name".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Fleet name.".to_string()),
                        },
                        SchemaField {
                            name: "state".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "One of `prepared`/`running`/`stopping`/`stopped`/`rolled_back`."
                                    .to_string(),
                            ),
                        },
                        SchemaField {
                            name: "panes".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: Some("Pane list with metadata.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "fleet_id".to_string(),
                    strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "describe_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same fleet_id on the same store produces identical \
                                      response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "describe_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the describe response \
                                      schema."
                            .to_string(),
                    },
                ],
            },
        ],
    }
}

// ============================================================================
// Context family — ft-hac7w.4 / BR-RC-ROBOT-CONTRACT.3
// ============================================================================

/// Schema-DSL declaration for the `context` family —
/// `status` / `rotate` / `history`. Per-pane conversation
/// context tracking integrating cass + session-resume.
///
/// Headline contract semantics:
///
/// - `status` is a **pure read**.
/// - `rotate` is **non-idempotent** — produces a fresh
///   `rotation_id` per call; the response is a TX-style
///   receipt naming what landed (which prior context_id was
///   archived, the new active one, etc.).
/// - `history` is a **pure read**.
/// - Concurrency: serializable per `pane_id`.
///
/// The `rotate` action's MustNotPartiallyMutate guarantee +
/// rotation_id receipt enables replay-after-failure: if a
/// caller doesn't get a response, they can re-issue with the
/// same caller_idempotency_key and the server returns the
/// same rotation_id (idempotent at the receipt-id level even
/// though the action itself produces a fresh state).
#[must_use]
pub fn context_family_contract() -> FamilyContract {
    FamilyContract {
        family_name: "context".to_string(),
        description: "Per-pane conversation context — status, rotate (TX-receipt), history."
            .to_string(),
        concurrency: ConcurrencyModel::PerPaneSerial,
        actions: vec![
            // ------------------------------------------------------------------
            // context status
            // ------------------------------------------------------------------
            ActionContract {
                action: "status".to_string(),
                robot_command: "robot context status".to_string(),
                mcp_tool_name: "ft.context.status".to_string(),
                description: "Snapshot the active context state for a pane.".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![SchemaField {
                        name: "pane_id".to_string(),
                        kind: SchemaKind::String,
                        required: true,
                        description: Some("Pane to look up.".to_string()),
                    }],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "pane_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "active_context_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "Currently-active context id; empty if pane has no context yet."
                                    .to_string(),
                            ),
                        },
                        SchemaField {
                            name: "depth".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some(
                                "Number of rotations in this pane's history.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "last_rotated_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Epoch ms of the most recent rotation.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![ProptestField {
                    name: "pane_id".to_string(),
                    strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                }],
                invariants: vec![
                    ContractInvariant {
                        name: "status_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same pane_id on the same store produces the same response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "status_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the status response schema."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // context rotate
            // ------------------------------------------------------------------
            ActionContract {
                action: "rotate".to_string(),
                robot_command: "robot context rotate".to_string(),
                mcp_tool_name: "ft.context.rotate".to_string(),
                description: "Archive the active context and start a fresh one. \
                              Non-idempotent — returns a TX-style receipt with a fresh \
                              rotation_id so failed calls can be retried by idempotency-key."
                    .to_string(),
                idempotency: IdempotencyClass::Sequential,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface {
                    events_emitted: vec!["context.rotated".to_string()],
                    storage_tables_mutated: vec![
                        "pane_contexts".to_string(),
                        "context_rotations".to_string(),
                    ],
                    ipc_targets: vec!["session_restore".to_string()],
                },
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "pane_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Pane whose context to rotate.".to_string()),
                        },
                        SchemaField {
                            name: "reason".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Optional human-readable rationale (e.g., 'compaction', \
                                 'manual')."
                                    .to_string(),
                            ),
                        },
                        SchemaField {
                            name: "caller_idempotency_key".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Caller-supplied idempotency key; same key returns the same \
                                 rotation_id."
                                    .to_string(),
                            ),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "rotation_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some(
                                "TX-style receipt id for replay — content-addressed.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "pane_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "previous_context_id".to_string(),
                            kind: SchemaKind::String,
                            required: false,
                            description: Some(
                                "Archived context id; absent for first rotation.".to_string(),
                            ),
                        },
                        SchemaField {
                            name: "new_context_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Newly-active context id.".to_string()),
                        },
                        SchemaField {
                            name: "rotated_at_ms".to_string(),
                            kind: SchemaKind::Integer,
                            required: true,
                            description: Some("Epoch ms.".to_string()),
                        },
                        SchemaField {
                            name: "is_replay".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some(
                                "True iff caller_idempotency_key matched a prior rotation \
                                 — same rotation_id returned."
                                    .to_string(),
                            ),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "pane_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "reason".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                    ProptestField {
                        name: "caller_idempotency_key".to_string(),
                        strategy: ProptestStrategyHint::OptionString { max_len: 32 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "rotate_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (pane_id, reason, caller_idempotency_key) against \
                                      the same starting state produces the same rotation_id \
                                      (idempotency-key replay)."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rotate_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the rotate response \
                                      schema."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rotate_atomic_on_failure".to_string(),
                        kind: InvariantKind::AtomicOnFailure,
                        description: "A failed rotate leaves no rows in pane_contexts / \
                                      context_rotations and emits no context.rotated event. \
                                      The caller can retry with the same idempotency_key."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rotate_idempotency_key_replay".to_string(),
                        kind: InvariantKind::Idempotence,
                        description: "Re-issuing rotate with the same caller_idempotency_key \
                                      returns the same rotation_id with is_replay=true and \
                                      no second context.rotated event."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "rotate_no_orphan_archived_context".to_string(),
                        kind: InvariantKind::Custom {
                            name: "rotate_no_orphan_archived_context".to_string(),
                        },
                        description: "Every entry in context_rotations references a row in \
                                      pane_contexts. Verified at the state-machine level."
                            .to_string(),
                    },
                ],
            },
            // ------------------------------------------------------------------
            // context history
            // ------------------------------------------------------------------
            ActionContract {
                action: "history".to_string(),
                robot_command: "robot context history".to_string(),
                mcp_tool_name: "ft.context.history".to_string(),
                description: "List past rotations for a pane (newest-first).".to_string(),
                idempotency: IdempotencyClass::Idempotent,
                failure_semantics: FailureSemantics::MustNotPartiallyMutate,
                side_effects: SideEffectSurface::read_only(),
                request_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "pane_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Pane whose history to list.".to_string()),
                        },
                        SchemaField {
                            name: "limit".to_string(),
                            kind: SchemaKind::Integer,
                            required: false,
                            description: Some("Cap on entries returned; default 100.".to_string()),
                        },
                    ],
                },
                response_schema: SchemaShape {
                    kind: SchemaKind::Object,
                    fields: vec![
                        SchemaField {
                            name: "pane_id".to_string(),
                            kind: SchemaKind::String,
                            required: true,
                            description: Some("Echo.".to_string()),
                        },
                        SchemaField {
                            name: "rotations".to_string(),
                            kind: SchemaKind::Array,
                            required: true,
                            description: Some("Rotation summaries, newest-first.".to_string()),
                        },
                        SchemaField {
                            name: "truncated".to_string(),
                            kind: SchemaKind::Boolean,
                            required: true,
                            description: Some("True iff `limit` clipped the result.".to_string()),
                        },
                    ],
                },
                request_proptest: vec![
                    ProptestField {
                        name: "pane_id".to_string(),
                        strategy: ProptestStrategyHint::AsciiString { max_len: 32 },
                    },
                    ProptestField {
                        name: "limit".to_string(),
                        strategy: ProptestStrategyHint::U32Range { min: 1, max: 1000 },
                    },
                ],
                invariants: vec![
                    ContractInvariant {
                        name: "history_is_deterministic".to_string(),
                        kind: InvariantKind::Determinism,
                        description: "Same (pane_id, limit) on the same store produces the \
                                      same response."
                            .to_string(),
                    },
                    ContractInvariant {
                        name: "history_response_shape".to_string(),
                        kind: InvariantKind::ResponseShape,
                        description: "Response data validates against the history response \
                                      schema."
                            .to_string(),
                    },
                ],
            },
        ],
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_family_contract_validates() {
        let contract = profile_family_contract();
        let errs = contract.validate();
        assert!(
            errs.is_empty(),
            "profile family contract violations: {errs:?}"
        );
    }

    #[test]
    fn profile_family_has_four_actions() {
        let contract = profile_family_contract();
        assert_eq!(contract.action_count(), 4);
        for action in ["list", "show", "apply", "validate"] {
            assert!(contract.action(action).is_some(), "missing action {action}");
        }
    }

    #[test]
    fn profile_family_emits_unique_mcp_tool_names() {
        let contract = profile_family_contract();
        let descriptors = contract.mcp_tool_descriptors();
        let mut names: Vec<&str> = descriptors.iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        let original_len = names.len();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate mcp tool names");
        assert_eq!(original_len, 4);
    }

    #[test]
    fn profile_family_json_schema_has_oneof_per_action() {
        let contract = profile_family_contract();
        let schema = contract.json_schema();
        let one_of = schema
            .pointer("/oneOf")
            .and_then(|v| v.as_array())
            .expect("oneOf array present at top level");
        assert_eq!(one_of.len(), 4);
        // Each branch pins `action` to a `const` so the discriminator
        // is unambiguous across overlapping params shapes.
        for branch in one_of {
            assert!(
                branch
                    .pointer("/properties/action/const")
                    .and_then(|v| v.as_str())
                    .is_some(),
                "every oneOf branch must constrain `action` to a const: {branch}"
            );
        }
    }

    #[test]
    fn profile_family_invariants_cover_required_kinds() {
        let contract = profile_family_contract();
        for action in &contract.actions {
            let kinds: Vec<&InvariantKind> = action.invariants.iter().map(|i| &i.kind).collect();
            assert!(
                kinds
                    .iter()
                    .any(|k| matches!(k, InvariantKind::Determinism)),
                "action {} missing Determinism",
                action.action
            );
            assert!(
                kinds
                    .iter()
                    .any(|k| matches!(k, InvariantKind::ResponseShape)),
                "action {} missing ResponseShape",
                action.action
            );
        }
        // The mutating action must also declare AtomicOnFailure.
        let apply = contract.action("apply").unwrap();
        let kinds: Vec<&InvariantKind> = apply.invariants.iter().map(|i| &i.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, InvariantKind::AtomicOnFailure)),
            "apply missing AtomicOnFailure"
        );
    }

    #[test]
    fn validation_catches_missing_determinism() {
        let mut contract = profile_family_contract();
        // Strip Determinism from `show`.
        let show = contract
            .actions
            .iter_mut()
            .find(|a| a.action == "show")
            .unwrap();
        show.invariants
            .retain(|i| !matches!(i.kind, InvariantKind::Determinism));
        let errs = contract.validate();
        assert!(
            errs.iter().any(|e| e.contains("Determinism")),
            "validate() should reject missing Determinism: {errs:?}"
        );
    }

    #[test]
    fn validation_catches_duplicate_action() {
        let mut contract = profile_family_contract();
        let dup = contract.actions[0].clone();
        contract.actions.push(dup);
        let errs = contract.validate();
        assert!(
            errs.iter().any(|e| e.contains("duplicate action name")),
            "validate() should reject duplicate action: {errs:?}"
        );
    }

    #[test]
    fn validation_catches_mutating_without_atomic_on_failure() {
        let mut contract = profile_family_contract();
        let apply = contract
            .actions
            .iter_mut()
            .find(|a| a.action == "apply")
            .unwrap();
        apply
            .invariants
            .retain(|i| !matches!(i.kind, InvariantKind::AtomicOnFailure));
        let errs = contract.validate();
        assert!(
            errs.iter().any(|e| e.contains("AtomicOnFailure")),
            "validate() should reject mutating action without AtomicOnFailure: {errs:?}"
        );
    }

    #[test]
    fn schema_shape_object_emits_required_array() {
        let shape = SchemaShape {
            kind: SchemaKind::Object,
            fields: vec![
                SchemaField {
                    name: "a".to_string(),
                    kind: SchemaKind::String,
                    required: true,
                    description: None,
                },
                SchemaField {
                    name: "b".to_string(),
                    kind: SchemaKind::Boolean,
                    required: false,
                    description: None,
                },
            ],
        };
        let schema = shape.to_json_schema();
        let required = schema
            .pointer("/required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], serde_json::Value::String("a".to_string()));
    }

    #[test]
    fn proptest_seeds_match_request_fields() {
        let contract = profile_family_contract();
        for action in &contract.actions {
            let schema_field_names: Vec<&str> = action
                .request_schema
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect();
            for seed in &action.request_proptest {
                assert!(
                    schema_field_names.contains(&seed.name.as_str()),
                    "action {}: proptest seed `{}` has no matching schema field",
                    action.action,
                    seed.name
                );
            }
        }
    }

    #[test]
    fn read_only_actions_have_empty_side_effects() {
        let contract = profile_family_contract();
        for name in ["list", "show", "validate"] {
            let action = contract.action(name).unwrap();
            assert!(
                action.side_effects.is_read_only(),
                "{name} should be read-only"
            );
        }
        let apply = contract.action("apply").unwrap();
        assert!(
            !apply.side_effects.is_read_only(),
            "apply must declare side-effects"
        );
    }

    #[test]
    fn family_invariants_enumerate_all_actions() {
        let contract = profile_family_contract();
        let invariants = contract.invariants();
        // 2 invariants × 3 read-only actions + 3 invariants for apply = 9
        assert_eq!(invariants.len(), 9);
    }

    #[test]
    fn family_contract_serde_roundtrips() {
        let contract = profile_family_contract();
        let json = serde_json::to_string(&contract).expect("serialize");
        let parsed: FamilyContract = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(contract, parsed);
    }
}
