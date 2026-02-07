# Integration Guide: wa Robot / MCP API

This guide shows how to build integrations on top of wa's robot and MCP
surfaces. It covers typed clients, JSON schemas, error handling, and
versioning.

## Surfaces

wa exposes two equivalent surfaces:

| Surface | Invocation | Transport | Use case |
|---------|-----------|-----------|----------|
| Robot CLI | `wa robot <cmd> --format json` | stdout JSON | Shell scripts, subprocess calls |
| MCP | `wa mcp serve` | stdio JSON-RPC | LLM tool-use, agent frameworks |

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
| `error_code` | string/null | Machine-readable code like `"WA-1003"` |
| `hint` | string/null | Actionable recovery suggestion |
| `elapsed_ms` | u64 | Wall-clock milliseconds the command took |
| `version` | string | wa version that produced this response |
| `now` | u64 | Unix epoch milliseconds when the response was generated |

Always check `ok` first. Never assume `data` is present on errors.

## Using the Typed Rust Client

The `wa_core::robot_types` module provides `Deserialize` types for all
response payloads. Add `wa-core` as a dependency:

```toml
[dependencies]
wa-core = { path = "../crates/wa-core" }
```

### Parse a response

```rust
use wa_core::robot_types::{RobotResponse, GetTextData, parse_response};

// From a string
let json = std::process::Command::new("wa")
    .args(["robot", "get-text", "--pane", "1", "--format", "json"])
    .output()
    .expect("wa failed");

let resp: RobotResponse<GetTextData> =
    RobotResponse::from_json_bytes(&json.stdout).unwrap();

match resp.into_result() {
    Ok(data) => println!("pane {} text: {}", data.pane_id, data.text),
    Err(e) => eprintln!("error: {}", e),
}
```

### Handle errors with codes

```rust
use wa_core::robot_types::{RobotResponse, SendData, ErrorCode};

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
            // Need to call wa robot approve first
        }
        _ => {
            // Use wa robot why <code> for explanation
        }
    }
}
```

### Untyped parsing

When the data type is not known at compile time:

```rust
use wa_core::robot_types::parse_response_untyped;

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
| `robot search` | `SearchData` | FTS5 search results with scores |
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
for optional fields, so they tolerate missing fields from older wa versions.

## JSON Schemas

Hand-authored JSON Schema Draft 2020-12 files live in `docs/json-schema/`.
Each schema describes the `data` field (not the envelope) for one endpoint.

### Schema Registry

The canonical mapping from endpoints to schemas is in
`wa_core::api_schema::SchemaRegistry::canonical()`. Each entry has:

```rust
EndpointMeta {
    id: "get_text",
    title: "Get Pane Text",
    description: "...",
    robot_command: Some("robot get-text"),
    mcp_tool: Some("wa.get_text"),
    schema_file: "wa-robot-get-text.json",
    stable: true,
    since: "0.1.0",
}
```

### Loading schemas at runtime

```rust
use wa_core::api_schema::SchemaRegistry;

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

wa uses `WA-xxxx` error codes organized by category:

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

- `WA-1005` (WezTerm connection refused)
- `WA-2001` (database locked)
- `WA-3003` (pattern match timeout)
- `WA-4002` (rate limit exceeded)
- `WA-6001` (network timeout)
- `WA-6002` (connection refused)

Use `ErrorCode::is_retryable()` to check programmatically.

### Getting help for an error

```bash
wa robot why WA-2001
```

Returns structured explanation with causes, recovery steps, and related codes.

## Versioning Policy

- wa follows semver for the `version` field.
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
use wa_core::api_schema::ApiVersion;

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
1. wa robot reserve --pane <id>        # claim a pane
2. wa robot get-text --pane <id>       # read current state
3. wa robot send --pane <id> --text .. # send commands
4. wa robot wait-for --pane <id> ..    # wait for result
5. wa robot events --pane <id>         # check for detections
6. wa robot release --pane <id>        # release the pane
```

All commands accept `--format json` for machine-readable output.

## Troubleshooting

### "data is null but ok is true"

This should not happen. If it does, the wa version may have a bug. Use
`parse_response_untyped()` to inspect the raw response.

### Deserialization fails on a new field

The typed client uses `#[serde(default)]` on all optional fields. If
deserialization fails, the wa version likely added a new required field.
Update your `wa-core` dependency.

### Schema validation fails

If you validate responses against JSON schemas and get failures:
1. Check if the schema file is in the `known_drift_schemas()` set.
2. The Rust types in `main.rs` are the source of truth; schemas may lag.
3. Run `cargo test -p wa-core --test typed_client_integration` to see the
   current drift report.
