# MCP API Spec (v1)

This document defines the MCP surface for wa (WezTerm Automata). The MCP API
mirrors `wa robot` for parity, stability, and token-efficient responses.

## Goals
- Stable, versioned surface for agent integrations.
- Token-efficient responses that match robot schemas.
- Minimal, complete tool set required to operate wa.

## Versioning
- `mcp_version`: MCP surface version (currently `v1`).
- `version`: wa semver (e.g., `0.1.0`).
- Changes are additive and backward-compatible within a major surface version.

## Response Envelope (v1)

All MCP tool calls return the same envelope:

```json
{
  "ok": true,
  "data": { "..." : "..." },
  "error": null,
  "error_code": null,
  "hint": null,
  "elapsed_ms": 12,
  "version": "0.1.0",
  "now": 1700000000000,
  "mcp_version": "v1"
}
```

Notes:
- When `ok=false`, `data` is omitted and `error` is populated.
- `data` MUST match the corresponding robot JSON schema under `docs/json-schema/`.
- `now` is epoch milliseconds.

## Tool List (v1)

All tools mirror `wa robot` semantics and schemas.

| Tool | Description | Data Schema |
|------|-------------|-------------|
| `wa.state` | Get current pane states | `docs/json-schema/wa-robot-state.json` |
| `wa.get_text` | Get text from a pane | `docs/json-schema/wa-robot-get-text.json` |
| `wa.send` | Send text to a pane | `docs/json-schema/wa-robot-send.json` |
| `wa.wait_for` | Wait for pattern match | `docs/json-schema/wa-robot-wait-for.json` |
| `wa.search` | FTS search across captures | `docs/json-schema/wa-robot-search.json` |
| `wa.events` | Query events | `docs/json-schema/wa-robot-events.json` |
| `wa.workflow_run` | Execute workflow | `docs/json-schema/wa-robot-workflow-run.json` |
| `wa.accounts` | List accounts | `docs/json-schema/wa-robot-accounts.json` |
| `wa.accounts_refresh` | Refresh account usage | `docs/json-schema/wa-robot-accounts-refresh.json` |
| `wa.rules_list` | List detection rules | `docs/json-schema/wa-robot-rules-list.json` |
| `wa.rules_test` | Test pattern matching | `docs/json-schema/wa-robot-rules-test.json` |
| `wa.reservations` | List active reservations | `docs/json-schema/wa-robot-reservations.json` |
| `wa.reserve` | Create reservation | `docs/json-schema/wa-robot-reserve.json` |
| `wa.release` | Release reservation | `docs/json-schema/wa-robot-release.json` |

### Tool Params (v1)

Parameter types use JSON primitives; `u64` fields are JSON numbers.

- `wa.state`
  - Params: `{ domain?: string, agent?: string, pane_id?: u64 }`

- `wa.get_text`
  - Params: `{ pane_id: u64, tail?: u64=50, escapes?: bool=false }`

- `wa.send`
  - Params: `{ pane_id: u64, text: string, dry_run?: bool=false, wait_for?: string, timeout_secs?: u64=30, wait_for_regex?: bool=false }`

- `wa.wait_for`
  - Params: `{ pane_id: u64, pattern: string, timeout_secs?: u64=30, tail?: u64=200, regex?: bool=false }`

- `wa.search`
  - Params: `{ query: string, limit?: u64=20, pane?: u64, since?: i64, snippets?: bool=false }`

- `wa.events`
  - Params: `{ limit?: u64=20, pane?: u64, rule_id?: string, event_type?: string, unhandled?: bool=false, since?: i64, would_handle?: bool=false, dry_run?: bool=false }`

- `wa.workflow_run`
  - Params: `{ name: string, pane_id: u64, force?: bool=false, dry_run?: bool=false }`

- `wa.accounts`
  - Params: `{ service?: string }`

- `wa.accounts_refresh`
  - Params: `{ service?: string }`

- `wa.rules_list`
  - Params: `{ pack?: string }`

- `wa.rules_test`
  - Params: `{ text: string, agent?: string }`

- `wa.reservations`
  - Params: `{ pane_id?: u64 }`

- `wa.reserve`
  - Params: `{ pane_id: u64, owner?: string, ttl_secs?: u64 }`

- `wa.release`
  - Params: `{ reservation_id: string }`

## Resource List (v1)

Resources are read-only snapshots. Query parameters mirror tool defaults.

- `wa://panes` — Current pane registry (same schema as `wa.state`)
- `wa://events` — Event feed (same schema as `wa.events`)
- `wa://accounts` — Account status (same schema as `wa.accounts`)
- `wa://workflows` — Available workflows
- `wa://rules` — Pattern rules (same schema as `wa.rules_list`)
- `wa://reservations` — Active reservations (same schema as `wa.reservations`)

## Error Codes (stable)

All MCP errors use stable codes prefixed with `WA-MCP-`:

| Error code | Meaning | Robot equivalent |
|------------|---------|------------------|
| `WA-MCP-0001` | Invalid arguments | `robot.invalid_args` |
| `WA-MCP-0002` | Unknown tool/resource | `robot.unknown_subcommand` |
| `WA-MCP-0003` | Config error | `robot.config_error` |
| `WA-MCP-0004` | WezTerm CLI error | `robot.wezterm_error` |
| `WA-MCP-0005` | Storage error | `robot.storage_error` |
| `WA-MCP-0006` | Policy denied | `robot.policy_denied` |
| `WA-MCP-0007` | Pane not found | `robot.pane_not_found` |
| `WA-MCP-0008` | Workflow error | `robot.workflow_error` |
| `WA-MCP-0009` | Timeout | `robot.timeout` |
| `WA-MCP-0010` | Not implemented | `robot.not_implemented` |

## Safety & Policy

Any tool that causes side effects MUST pass the PolicyEngine, including:
- `wa.send`
- `wa.workflow_run`
- `wa.reserve` / `wa.release`
- `wa.accounts_refresh` (if it triggers external calls)

Resources are read-only and MUST not cause side effects.

## Parity & Schema Contract

The MCP surface is a thin wrapper over robot mode. For each tool:
- Input parameters map 1:1 with the robot command.
- Output `data` must validate against the matching robot JSON schema.
- Errors must map to stable MCP error codes above.
