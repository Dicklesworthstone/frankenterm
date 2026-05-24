# Pattern Pack File Format (v1)

> Authoritative spec for the YAML / JSON / TOML files that
> `crates/frankenterm-core/src/patterns.rs` parses into a
> `PatternPack`. External rule authors and security auditors should
> read this document — not the loader source — to learn the format.
>
> Tracked under bead **ft-b35tw**. The companion JSON Schema lives at
> [`docs/json-schema/ft-pattern-pack.json`](json-schema/ft-pattern-pack.json).

## At a glance

A pattern pack is a single document with three top-level fields:

```yaml
name: builtin:core
version: "1.0.0"
rules:
  - id: codex.usage.reached
    agent_type: codex
    event_type: usage_limit
    severity: warning
    anchors: ["usage limit", "Please try again"]
    regex: 'usage limit for the (?P<window_hours>\d+)h window'
    description: Codex hit its rolling-window quota.
    remediation: Wait for the quota to reset or rotate accounts.
```

Discovery rules:

- **Built-in packs** are compiled into the binary and loaded
  unconditionally.
- **User packs** under `.ft/patterns/` (any depth) are auto-discovered
  via `discover_packs_from_dir`. The discovery walker rejects any
  symlink whose canonical target escapes the workspace root (fix
  landed at commit `6ab52765`).
- **Explicit packs** named in `[patterns]` config are loaded from
  the documented path.

Supported extensions (case-insensitive): `.yaml`, `.yml`, `.json`,
`.toml`. The extension determines the parser; the on-disk content
shape is the same across all three.

## Top-level fields

| Field | Type | Presence | Description |
|-------|------|:--------:|-------------|
| `name` | string | **MUST** | Pack identifier. Built-in packs use a `builtin:` prefix (`builtin:core`); user packs use a plain or namespaced name (`example.com:custom-rules`). MUST be non-empty after trimming. |
| `version` | string | **MUST** | Pack version. Convention is semver-like (`"1.0.0"`) but the loader treats it as opaque; MUST be non-empty after trimming. |
| `rules` | array of [Rule](#rule-fields) | **MUST** | The rule list. MAY be empty for an intentionally-stubbed pack, but every rule's `id` MUST be unique within the pack. |

Unknown top-level fields are tolerated by the parser (default serde
behavior) but are not part of the spec — authors SHOULD NOT rely on
them being preserved.

## Rule fields

A rule (Rust: `RuleDef`) has the following fields. `MUST` /
`SHOULD` / `MAY` follows RFC 2119 semantics.

| Field | Type | Presence | Description |
|-------|------|:--------:|-------------|
| `id` | string | **MUST** | Stable rule identifier (e.g. `codex.usage.reached`). MUST be unique within the pack. Authors SHOULD use a `<agent>.<feature>.<event>` dotted convention. |
| `agent_type` | enum | **MUST** | One of `codex`, `claude_code`, `gemini`, `wezterm`, `unknown`. Serde rename is `snake_case`. |
| `event_type` | string | **MUST** | The event-type label emitted on match (e.g. `usage_limit`, `compaction_required`). MUST be non-empty after trimming. |
| `severity` | enum | **MUST** | One of `info`, `warning`, `critical`. Serde rename is `snake_case`. |
| `anchors` | array of string | **MUST** | Literal substrings used by the Aho-Corasick quick-reject pass. MUST include at least one non-empty anchor. |
| `regex` | string | MAY | Optional extraction regex. Named captures (`(?P<name>...)`) are preferred; unnamed groups MAY be used but are not surfaced as `extracted` fields. |
| `description` | string | **MUST** | Human-readable description shown in `ft why <rule_id>` and the doctor pack listing. MUST be non-empty after trimming. |
| `remediation` | string | MAY | Suggested remediation text. Surfaced in the rendered event template. |
| `workflow` | string | MAY | Suggested workflow name to invoke (e.g. `handle_usage_limits`). |
| `manual_fix` | string | MAY | Manual fix instructions for environments where workflow execution is unavailable. Stored as `Option<String>`; absent in the serialized form when null. |
| `preview_command` | string | MAY | Preview command template supporting `{pane}`, `{event_id}`, `{agent}` interpolation. |
| `learn_more_url` | string | MAY | URL for additional documentation about this rule. |

Unknown rule fields are tolerated by the parser (silent drop), but
the conformance gate enforces the JSON Schema `additionalProperties`
contract: pack files MUST NOT carry unknown rule fields. CI treats
unknown rule fields as a build failure even though the lower-level
serde parser can still deserialize them.

## Validation

`PatternPack::validate()` runs in two flavours:

1. **Built-in pack validation** (default for packs loaded via
   `[patterns]`). Each rule's regex is validated as Rust-regex
   syntax; anchors are checked for non-emptiness; rule IDs are
   checked for uniqueness within the pack.
2. **User-pack validation** (`validate_as_user_pack`). Same rules
   PLUS additional safety checks: rule IDs MUST start with a
   namespace prefix (no bare `core.*` or `builtin:*` in user
   packs); regexes MUST NOT contain unbounded backreferences
   (`\1`+) that could enable catastrophic-backtracking DoS.

A pack that passes the JSON Schema check MAY still fail validation —
the schema is a structural gate, not a semantic one.

## Versioning policy

The format itself does not (yet) carry a `format_version` field. The
file format has been **additive-only since v1**:

- New OPTIONAL rule fields (e.g. `manual_fix`, `preview_command`,
  `learn_more_url` in recent commits) are backward-compatible.
- A REQUIRED field rename or removal is a breaking change and MUST
  be guarded by a `format_version` field bump (deferred until the
  first such change is needed).
- Enum widening (new `agent_type` or `severity` variants) is a
  compatibility break for the current first-party loader. The Rust
  enums are serde-tagged without an `other` catch-all, so unknown
  variants fail deserialization instead of being coerced to
  `unknown`. The `unknown` agent type is a literal supported value,
  not a fallback for unrecognized strings.

When the first breaking change ships, this spec gains a
`format_version` row in the top-level table; older packs continue to
load by treating absent `format_version` as `1`.

## Discovery + sandbox guarantees

- `discover_packs_from_dir` (sandboxed via `sandbox_resolve_dir`)
  rejects any path whose canonical resolution escapes the workspace
  root. `..` traversal and symlink-escape are both blocked in a
  single `fs::canonicalize` call followed by a prefix check
  (commits `0239fbd6`, `37384b01`, `6ab52765`).
- Pack files outside the discovery roots are loaded only when
  named explicitly in `[patterns]` config; the loader does NOT
  walk `$HOME` or system directories.
- A malformed YAML/JSON/TOML file fails fast with
  `PatternError::InvalidRule` carrying the parser error string;
  the loader does NOT silently ignore unparseable files.

## Reference fixtures

`crates/frankenterm-core/tests/fixtures/pattern_packs/` ships a
fixture corpus mirroring this spec:

- `valid/minimal_pack.yaml` — smallest legal pack (one rule, only
  required fields).
- `valid/full_pack.yaml` — every documented field exercised at least
  once.
- `valid/multi_rule_pack.json` — multiple rules in JSON form.
- `valid/toml_pack.toml` — TOML round-trip example.
- `invalid/duplicate_rule_id.yaml` — two rules with the same `id`.
- `invalid/missing_severity.yaml` — rule with no `severity`.
- `invalid/empty_name.yaml` — pack with `name: ""`.

The conformance test
(`tests/conformance_pattern_pack_format.rs`) loads each valid
fixture, validates against the JSON Schema, asserts a successful
`PatternPack` parse + `validate()`, and confirms each invalid fixture
fails with the documented error.

## Coverage matrix

The conformance test enumerates every Rule field below and asserts
each is exercised by at least one valid fixture. New OPTIONAL
fields landing in `RuleDef` without a fixture update fail the
matrix check.

## References

- `crates/frankenterm-core/src/patterns.rs:619` — `PatternPack`
- `crates/frankenterm-core/src/patterns.rs:487` — `RuleDef`
- `docs/json-schema/ft-pattern-pack.json` — companion JSON Schema
- `tests/conformance_pattern_pack_format.rs` — CI gate
- Sandbox commits: `6ab52765`, `0239fbd6`, `37384b01`
- Bead: ft-b35tw
