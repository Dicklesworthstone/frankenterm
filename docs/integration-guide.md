# Integration Guide: ft Robot / MCP API

This guide shows how to build integrations on top of ft's robot and MCP
surfaces. It covers typed clients, JSON schemas, error handling, and
versioning.

## Surfaces

ft exposes two equivalent surfaces:

| Surface | Invocation | Transport | Use case |
|---------|-----------|-----------|----------|
| Robot CLI | `ft robot <cmd> --format json` | stdout JSON | Shell scripts, subprocess calls |
| MCP | `ft mcp serve` | stdio JSON-RPC | LLM tool-use, agent frameworks |

Both return the same response envelope and data schemas.

## Response Envelope

Every response is wrapped in a standard envelope:

```json
{
  "ok": true,
  "data": { ... },
  "error": null,
  "error_code": null,
  "hint": null,
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": 1700000000000
}
```

| Field | Type | Description |
|-------|------|-------------|
| `ok` | bool | `true` on success, `false` on error |
| `data` | object/null | Command-specific payload (present when `ok == true`) |
| `error` | string/null | Human-readable error message |
| `error_code` | string/null | Machine-readable code like `"FT-1003"` |
| `hint` | string/null | Actionable recovery suggestion |
| `elapsed_ms` | u64 | Wall-clock milliseconds the command took |
| `version` | string | ft version that produced this response |
| `now` | u64 | Unix epoch milliseconds when the response was generated |

Always check `ok` first. Never assume `data` is present on errors.

## Using the Typed Rust Client

The `frankenterm_core::robot_types` module provides `Deserialize` types for all
response payloads. Add `frankenterm-core` as a dependency:

```toml
[dependencies]
frankenterm-core = { path = "../crates/frankenterm-core" }
```

### Parse a response

```rust
use frankenterm_core::robot_types::{RobotResponse, GetTextData};

// From a string
let json = std::process::Command::new("ft")
    .args(["robot", "-f", "json", "get-text", "1", "--tail", "100"])
    .output()
    .expect("ft failed");

let resp: RobotResponse<GetTextData> =
    RobotResponse::from_json_bytes(&json.stdout).unwrap();

match resp.into_result() {
    Ok(data) => println!("pane {} text: {}", data.pane_id, data.text),
    Err(e) => eprintln!("error: {}", e),
}
```

### Handle errors with codes

```rust
use frankenterm_core::robot_types::{RobotResponse, SendData, ErrorCode};

let resp: RobotResponse<SendData> = /* parse response */;

if let Some(code) = resp.parsed_error_code() {
    match code {
        ErrorCode::RateLimitExceeded | ErrorCode::DatabaseLocked => {
            // Retryable - back off and retry
            assert!(code.is_retryable());
        }
        ErrorCode::ActionDenied => {
            // Policy blocked this action
        }
        ErrorCode::ApprovalRequired => {
            // Need to call ft robot approve first
        }
        _ => {
            // Use ft robot why <code> for explanation
        }
    }
}
```

### Untyped parsing

When the data type is not known at compile time:

```rust
use frankenterm_core::robot_types::parse_response_untyped;

let resp = parse_response_untyped(json_str).unwrap();
if resp.ok {
    let data = resp.data.unwrap();
    // Access fields dynamically
    println!("{}", data["pane_id"]);
}
```

## Available Data Types

Each robot command has a corresponding typed struct:

| Command | Type | Description |
|---------|------|-------------|
| `robot get-text` | `GetTextData` | Pane text with truncation info |
| `robot send` | `SendData` | Injection result with policy decision |
| `robot wait-for` | `WaitForData` | Pattern match polling result |
| `robot search` | `SearchData` | Lexical/semantic/hybrid search results with mode-aware scores |
| `robot events` | `EventsData` | Detected events with filters |
| `robot events annotate/triage/label` | `EventMutationData` | Annotation mutation result |
| `robot workflow run` | `WorkflowRunData` | Workflow execution start |
| `robot workflow list` | `WorkflowListData` | Available workflows |
| `robot workflow status` | `WorkflowStatusData` | Execution progress |
| `robot workflow abort` | `WorkflowAbortData` | Abort confirmation |
| `robot rules list` | `RulesListData` | Rule pack listing |
| `robot rules test` | `RulesTestData` | Rule match testing |
| `robot rules show` | `RuleDetailData` | Full rule details |
| `robot rules lint` | `RulesLintData` | Lint results |
| `robot accounts list` | `AccountsListData` | Account balances |
| `robot accounts refresh` | `AccountsRefreshData` | Refresh result |
| `robot reservations list` | `ReservationsListData` | Pane reservations |
| `robot reserve` | `ReserveData` | New reservation |
| `robot release` | `ReleaseData` | Release confirmation |
| `robot approve` | `ApproveData` | Approval validation |
| `robot why` | `WhyData` | Error code explanation |
| `robot help` | `QuickStartData` | Quick-start guide |

All types derive `Serialize` + `Deserialize` and use `#[serde(default)]`
for optional fields, so they tolerate missing fields from older ft versions.

### Search mode fields

`SearchData`/`SearchHit` include mode-aware optional fields:

- `SearchData.mode`: effective mode label (`lexical`, `semantic`, `hybrid`)
- `SearchData.metrics`: optional pipeline metrics payload (semantic/hybrid MCP responses)
- `SearchHit.semantic_score`: optional semantic similarity score
- `SearchHit.fusion_rank`: optional 0-based rank from fused ordering
- `SearchData.query`, `SearchHit.snippet`, and `SearchHit.content` are redacted output fields (safe to log by default).
- Search/read calls can fail with policy decisions (`ErrorCode::ActionDenied`, `ErrorCode::ApprovalRequired`).

## JSON Schemas

Hand-authored JSON Schema Draft 2020-12 files live in `docs/json-schema/`.
Each schema describes the `data` field (not the envelope) for one endpoint.

### Schema Registry

The canonical mapping from endpoints to schemas is in
`frankenterm_core::api_schema::SchemaRegistry::canonical()`. Each entry has:

```rust
EndpointMeta {
    id: "get_text",
    title: "Get Pane Text",
    description: "...",
    robot_command: Some("robot get-text"),
    // Note: MCP tool names are currently legacy-prefixed with `wa.*`.
    mcp_tool: Some("wa.get_text"),
    schema_file: "wa-robot-get-text.json",
    stable: true,
    since: "0.1.0",
}
```

### Loading schemas at runtime

```rust
use frankenterm_core::api_schema::SchemaRegistry;

let registry = SchemaRegistry::canonical();
for endpoint in &registry.endpoints {
    println!("{}: {} (stable: {})", endpoint.id, endpoint.schema_file, endpoint.stable);
}
```

### Known drift

Some schemas were hand-authored before the Rust types stabilized and have
field naming differences. The integration test suite
(`tests/typed_client_integration.rs`) documents 12 known drift entries. When
schemas are updated to match the Rust implementation, the drift entries will
be removed and the tests will enforce compatibility.

## Error Codes

ft uses `FT-xxxx` error codes organized by category:

| Range | Category | Examples |
|-------|----------|----------|
| 1xxx | WezTerm | CLI not found, pane not found, connection refused |
| 2xxx | Storage | Database locked, corruption, FTS error, disk full |
| 3xxx | Pattern | Invalid regex, rule pack not found, match timeout |
| 4xxx | Policy | Action denied, rate limited, approval required/expired |
| 5xxx | Workflow | Not found, step failed, timeout, already running |
| 6xxx | Network | Timeout, connection refused |
| 7xxx | Config | Invalid config, config not found |
| 9xxx | Internal | Internal error, feature not available, version mismatch |

### Retryable errors

The following codes are safe to retry with backoff:

- `FT-1005` (WezTerm connection refused)
- `FT-2001` (database locked)
- `FT-3003` (pattern match timeout)
- `FT-4002` (rate limit exceeded)
- `FT-6001` (network timeout)
- `FT-6002` (connection refused)

Use `ErrorCode::is_retryable()` to check programmatically.

### Getting help for an error

```bash
ft robot why FT-2001
```

Returns structured explanation with causes, recovery steps, and related codes.

## Versioning Policy

- ft follows semver for the `version` field.
- The `SchemaRegistry` tracks `since` (version when endpoint was added) and
  `stable` (whether the endpoint's contract is frozen).
- Within a major version:
  - New optional fields may be added (additive changes).
  - Required fields are never removed.
  - Field types are never changed.
- Breaking changes bump the major version and are tracked in the schema
  registry's `SchemaDiffResult`.

### Checking compatibility

```rust
use frankenterm_core::api_schema::ApiVersion;

let client_version = ApiVersion::parse("0.1.0").unwrap();
let server_version = ApiVersion::parse("0.2.0").unwrap();

match client_version.check_compatibility(&server_version) {
    VersionCompatibility::Exact | VersionCompatibility::Compatible => { /* ok */ }
    VersionCompatibility::NewerMinor => { /* server has new features */ }
    VersionCompatibility::Incompatible => { /* major version mismatch */ }
}
```

## Typical Integration Loop

A robot-mode agent typically follows this loop:

```
1. ft robot reservations reserve <pane_id> --owner-id <agent>   # claim a pane
2. ft robot get-text <pane_id> --tail 200                        # read current state
3. ft robot send <pane_id> "<text>"                              # send commands
4. ft robot wait-for <pane_id> "<pattern>"                       # wait for result
5. ft robot events --pane <pane_id>                              # check for detections
6. ft robot reservations list                                    # find reservation_id
7. ft robot reservations release <reservation_id>                # release the pane
```

All commands accept `--format json` for machine-readable output.

## Troubleshooting

### "data is null but ok is true"

This should not happen. If it does, the ft version may have a bug. Use
`parse_response_untyped()` to inspect the raw response.

### Deserialization fails on a new field

The typed client uses `#[serde(default)]` on all optional fields. If
deserialization fails, the ft version likely added a new required field.
Update your `frankenterm-core` dependency.

### Schema validation fails

If you validate responses against JSON schemas and get failures:
1. Check if the schema file is in the `known_drift_schemas()` set.
2. The Rust types in `main.rs` are the source of truth; schemas may lag.
3. Run `cargo test -p frankenterm-core --test typed_client_integration` to see the
   current drift report.
