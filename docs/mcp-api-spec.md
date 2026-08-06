# MCP API Spec (v1)

This document defines the MCP surface for ft (FrankenTerm). MCP and `ft robot`
share the stable envelope and many payload schemas, but they are distinct
transports with explicitly documented capability differences.

## Goals
- Stable, versioned surface for agent integrations.
- Token-efficient responses with schema-validated shared or MCP-specific data.
- Minimal, complete tool set required to operate ft (FrankenTerm).

## Running the MCP Server

The MCP server is feature-gated. Build with MCP enabled and run over stdio:

```bash
cargo build --profile release-interactive --features mcp
ft mcp serve
```

## Versioning
- `mcp_version`: MCP surface version (currently `v1`).
- `version`: ft semver (e.g., `0.1.0`).
- Changes are additive and backward-compatible within a major surface version.

## Response Envelope (v1)

By default (`format=json`), MCP tool calls return this JSON envelope:

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
- When the tool table names a JSON schema, `data` MUST match that schema. <!-- MCP-V1-001 --> Tools
  labeled `Inline` use the named Rust serde contract until a dedicated schema is
  published.
- `now` is epoch milliseconds.
- Tools also accept optional `format: "json" | "toon"` in params:
  - `format=json` (default): response text is JSON envelope as shown above.
  - `format=toon`: response text is TOON-encoded envelope with the same logical fields.

## Tool List (v1)

The table records schema correspondence; it does not imply identical transport,
streaming, condition-source, or delivery-acknowledgment semantics.

Note: Tool IDs currently still use the legacy `wa.*` prefix and resources use the
legacy `wa://...` scheme for backward compatibility.

| Tool | Description | Data Schema |
|------|-------------|-------------|
| `wa.state` | Get current pane states | `docs/json-schema/wa-robot-state.json` |
| `wa.get_text` | Get text from a pane | `docs/json-schema/wa-robot-get-text.json` |
| `wa.send` | Send text to a pane | `docs/json-schema/wa-robot-send.json` |
| `wa.wait_for` | Wait for pattern match | `docs/json-schema/wa-robot-wait-for.json` |
| `wa.search` | Unified lexical/semantic/hybrid search across captures | `docs/json-schema/wa-robot-search.json` |
| `wa.events` | Query events | `docs/json-schema/wa-robot-events.json` |
| `wa.await_event` | Long-poll for persisted events, with optional delivery claims | Inline (`McpAwaitEventData`) |
| `wa.events_annotate` | Set/clear an event note | `docs/json-schema/wa-robot-event-mutation.json` |
| `wa.events_triage` | Set/clear an event triage state | `docs/json-schema/wa-robot-event-mutation.json` |
| `wa.events_label` | Add/remove/list event labels | `docs/json-schema/wa-robot-event-mutation.json` |
| `wa.workflow_run` | Execute workflow | `docs/json-schema/wa-robot-workflow-run.json` |
| `wa.tx_plan` | Validate and summarize mission tx contract metadata | Inline (`McpTxPlanData`) |
| `wa.tx_run` | Execute tx prepare+commit (+compensation on partial failure) | Inline (`McpTxRunData`) |
| `wa.tx_rollback` | Execute compensation phase for committed tx steps | Inline (`McpTxRollbackData`) |
| `wa.tx_show` | Inspect tx lifecycle, receipts, and legal transitions | Inline (`McpTxShowData`) |
| `wa.mission_state` | Query mission lifecycle, assignments, and counters | Inline (`McpMissionStateData`) |
| `wa.mission_explain` | Show legal transitions, failure catalog, assignment context | Inline (`McpMissionExplainData`) |
| `wa.mission_pause` | Pause active mission with checkpoint | Inline (`McpMissionControlData`) |
| `wa.mission_resume` | Resume paused mission, restore prior state | Inline (`McpMissionControlData`) |
| `wa.mission_abort` | Abort mission, cancel in-flight assignments | Inline (`McpMissionControlData`) |
| `wa.accounts` | List accounts | `docs/json-schema/wa-robot-accounts.json` |
| `wa.accounts_refresh` | Refresh account usage | `docs/json-schema/wa-robot-accounts-refresh.json` |
| `wa.rules_list` | List detection rules | `docs/json-schema/wa-robot-rules-list.json` |
| `wa.rules_test` | Test pattern matching | `docs/json-schema/wa-robot-rules-test.json` |
| `wa.workflow_list` | List available workflows | `docs/json-schema/wa-robot-workflow-list.json` |
| `wa.workflow_status` | Check workflow execution status | `docs/json-schema/wa-robot-workflow-status.json` |
| `wa.workflow_abort` | Abort a running workflow | `docs/json-schema/wa-robot-workflow-abort.json` |
| `wa.approve` | Submit approval code for pending action | `docs/json-schema/wa-robot-approve.json` |
| `wa.why` | Explain an error code or policy denial | `docs/json-schema/wa-robot-why.json` |
| `wa.rules_show` | Show details for a specific rule | `docs/json-schema/wa-robot-rules-show.json` |
| `wa.rules_lint` | Lint rules: validate IDs, fixtures, regex | `docs/json-schema/wa-robot-rules-lint.json` |
| `wa.reservations` | List active reservations | `docs/json-schema/wa-robot-reservations.json` |
| `wa.reserve` | Create reservation | `docs/json-schema/wa-robot-reserve.json` |
| `wa.release` | Release reservation | `docs/json-schema/wa-robot-release.json` |

### Tool Params (v1)

Parameter types use JSON primitives; `u64` fields are JSON numbers.
All tools accept an optional `format?: "json" | "toon"` parameter (default: `json`).

- `wa.state`
  - Params: `{ domain?: string, agent?: string, pane_id?: u64 }`

- `wa.get_text`
  - Params: `{ pane_id: u64, tail?: u64=50, escapes?: bool=false }`
  - Response notes: text payloads are redacted before serialization; policy gates may return `FT-MCP-0006`.

- `wa.send`
  - Params: `{ pane_id: u64, text: string, dry_run?: bool=false, wait_for?: string, timeout_secs?: u64=30, wait_for_regex?: bool=false }`
  - Response notes: non-dry-run send responses may include `data.submit`, a durable submit receipt keyed by `idempotency_key` and aligned with the audit `correlation_id`.

- `wa.wait_for`
  - Params: `{ pane_id: u64, pattern: string, timeout_secs?: u64=30, tail?: u64=200, regex?: bool=false }`

- `wa.search`
  - Params: `{ query: string, limit?: u64=20, pane?: u64, since?: i64, until?: i64, snippets?: bool=true, mode?: "lexical"|"semantic"|"hybrid"="lexical" }`
  - Response notes: `data.mode` reports effective mode and `data.metrics` may include fusion/cache/budget telemetry for semantic/hybrid runs.
  - Redaction/policy: response `query`/`snippet`/`content` fields are redacted; denied/approval-required outcomes return `FT-MCP-0006` (approval-required includes a hint command).

- `wa.events`
  - Params: `{ limit?: u64=20, pane?: u64, rule_id?: string, event_type?: string, triage_state?: string, label?: string, unhandled?: bool=false, since?: nonnegative_i64 }`
  - This tool is a bounded newest-first snapshot, not a resumable cursor surface. Successful `data` always includes `cursor_capability="non_resumable_newest_first_snapshot"`. Use `wa.await_event` when a storage-authoritative cursor is required.

- `wa.await_event`
  - Params: `{ any?: string[], all?: string[], timeout_secs?: u64=30, poll_interval_ms?: u64=250, cursor?: i64, cursor_epoch?: 32-lowercase-hex, cursor_scope?: 64-lowercase-hex, pane?: u64, unhandled?: bool=false, claim?: bool=false, checkpoint_only?: bool=false }`
  - At least one nonempty `any` or `all` set is required. Each condition set is bounded to 16 entries and each condition to 256 UTF-8 bytes. This MCP tool currently supports only `rule:<glob>` conditions; CLI-only quiescence and live pane-state conditions are not accepted. `timeout_secs` is bounded to `1..=300`.
  - A durable resume token is the complete `cursor`/`cursor_epoch`/`cursor_scope` triple. The scope binds the canonical condition sets, pane filter, effective unhandled filter, claim mode, and the explicit DB-events-only quiescence capability marker. Partial, malformed, wrong-scope, stale-epoch, pruned, or ahead-of-authority tokens fail closed with `FT-MCP-0016`.
  - With no cursor, the first atomic storage page establishes the current durable tail; history at or below that tail is not replayed. `checkpoint_only=true` returns immediately with `bootstrap_state="storage_tail_checkpoint"` and the fresh `final_cursor`/`final_cursor_epoch`/`final_cursor_scope` triple. Bootstrap requires `claim=false` and no input cursor.
  - Normal responses return a `type="await_result"`, `final_cursor` triple, condition status arrays, matched events, `pending_finalize`, and optionally `candidate_cursor`, `claim_delivery`, and `bootstrap_state`. `candidate_cursor` is diagnostic pending state and is never a resumable token.
  - With `claim=true`, matched rows are atomically leased. When a satisfied result retains at least one locally owned lease, the response truthfully shows pre-finalization handled state, sets `pending_finalize=true`, exposes the highest pending `candidate_cursor`, and sets `claim_delivery="pending_finalize_after_delivery_ack"`. The committed `final_cursor` never crosses the earliest pending or foreign lease. After the complete requested-format response is successfully written/flushed at the sender-side transport boundary, a bounded completion queue finalizes the local leases in exact batches of at most 64 without blocking the transport loop. A batch ownership conflict rolls back that batch before deadline-bounded per-lease CAS recovery. This acknowledgment does **not** prove that the client parsed, persisted, or acted on the response.
  - A timeout/error after a partial composite match may also have local leases. Its public cursor returns to the safe pre-latch boundary, and the same bounded completion queue releases those leases asynchronously in exact batches, with the same per-lease conflict recovery. Known serialization or delivery failure uses that queue as well. Queue saturation, disconnection, worker shutdown, storage-open failure, or token-CAS failure can make an immediate retry transiently observe the reservation; durable lease expiry remains the recovery authority. Such nonterminal results keep `pending_finalize=false` and omit `candidate_cursor` and `claim_delivery` because no advancement is being promised.
  - Within one MCP server lifetime, `wa.await_event` uses one shared asupersync runtime and one `StorageHandle`/writer per healthy database epoch for request scans/claims and post-response completion. The service admits at most 16 total await requests, runs at most 8 concurrently, and maintains a separate bounded completion lane with at most 64 queued jobs plus one job in flight. Completion scheduling is prioritized before request-slot refill. New admission returns retryable `FT-MCP-0003` when the 16-request cap is full or while shared storage is initializing, reconnecting, or shutting down.
  - A storage-epoch failure cancels only active requests bound to that epoch, rejects queued work admitted for that epoch, retains durable lease expiry as the crash-recovery authority, and reconnects with bounded 50 ms to 5 s backoff without rebuilding the runtime. Request-local cancellation, cursor-discontinuity, capacity, and ambiguity errors do not invalidate the shared storage epoch. The transport's same-connection serialization limit still applies.
  - A foreign live lease retains the exposed cursor before that event independently of whether another event later satisfies the same condition, but does not prevent a monotonic scan from finding later claimable matches. Retried holes are refetched by exact event ID, and retried holes plus later matches are returned in ascending event-ID order.
  - Blocked-hole tracking retains at most 500 compact entries. If more concurrently leased matching rows are encountered, the request fails closed with `FT-MCP-0005` before advancing past the first untracked hole; retry from the same input cursor after competing claims complete.
  - Every explicit token is reconciled against the current retention epoch and exact deletion ledger before its cursor can be returned or advanced. A fresh request starts only at the successfully acquired atomic storage tail; process-entry time is not cursor authority.

- `wa.events_annotate`
  - Params: `{ event_id: i64, note?: string, clear?: bool=false, by?: string }`

- `wa.events_triage`
  - Params: `{ event_id: i64, state?: string, clear?: bool=false, by?: string }`

- `wa.events_label`
  - Params: `{ event_id: i64, add?: string, remove?: string, list?: bool=false, by?: string }`

- `wa.workflow_run`
  - Params: `{ name: string, pane_id: u64, force?: bool=false, dry_run?: bool=false }`

- `wa.tx_plan`
  - Params: `{ contract_file?: string }`

- `wa.tx_run`
  - Params: `{ contract_file?: string, fail_step?: string, paused?: bool=false, kill_switch?: "off"|"safe_mode"|"hard_stop" }`

- `wa.tx_rollback`
  - Params: `{ contract_file?: string, fail_compensation_for_step?: string }`

- `wa.tx_show`
  - Params: `{ contract_file?: string, include_contract?: bool=false }`

- `wa.mission_state`
  - Params: `{ mission_file?: string, mission_state?: string, run_state?: string, agent_state?: string, action_state?: string, assignment_id?: string, assignee?: string, limit?: integer }`

- `wa.mission_explain`
  - Params: `{ mission_file?: string, assignment_id?: string }`

- `wa.mission_pause`
  - Params: `{ mission_file?: string, reason: string, requested_by?: string="mcp-agent" }`

- `wa.mission_resume`
  - Params: `{ mission_file?: string, requested_by?: string="mcp-agent" }`

- `wa.mission_abort`
  - Params: `{ mission_file?: string, reason: string, requested_by?: string="mcp-agent", error_code?: string }`

- `wa.accounts`
  - Params: `{ service?: string }`

- `wa.accounts_refresh`
  - Params: `{ service?: string }`

- `wa.rules_list`
  - Params: `{ pack?: string }`

- `wa.rules_test`
  - Params: `{ text: string, agent?: string }`

- `wa.rules_show`
  - Params: `{ rule_id: string }`

- `wa.rules_lint`
  - Params: `{ pack?: string, fixtures?: bool=false, strict?: bool=false }`

- `wa.workflow_list`
  - Params: `{}`

- `wa.workflow_status`
  - Params: `{ execution_id?: string, pane_id?: u64, active?: bool=false, verbose?: bool=false }`
  - Note: At least one of `execution_id`, `pane_id`, or `active` must be provided.

- `wa.workflow_abort`
  - Params: `{ execution_id: string, reason?: string, force?: bool=false }`

- `wa.approve`
  - Params: `{ code: string, pane_id?: u64, fingerprint?: string, dry_run?: bool=false }`

- `wa.why`
  - Params: `{ code: string }`

- `wa.reservations`
  - Params: `{ pane_id?: u64 }`

- `wa.reserve`
  - Params: `{ pane_id: u64, owner?: string, ttl_secs?: u64 }`

- `wa.release`
  - Params: `{ reservation_id: string }`

### Long-poll transport limitation

The current FastMCP stdio loop processes one inbound message at a time. An
active `wa.await_event` therefore queues unrelated requests on the same MCP
connection until it matches, times out, or its request `Cx` is cancelled by the
server. Use a dedicated MCP connection for long polls when other control calls
must remain responsive.

FastMCP's same sequential receive loop also cannot read a JSON-RPC
`notifications/cancelled` message while its corresponding handler is still
running. The notification is processed only after the long poll returns, so it
is not currently a prompt client-side cancellation mechanism. FrankenTerm
still threads and checks the request `Cx`, bounds cancellation polling to 50 ms
once cancellation is visible inside the server, and gives storage-backed mode
a 330-second framework deadline for the tool's 300-second maximum plus response
delivery margin. Do not interpret direct `Cx` cancellation tests as proof of
same-connection JSON-RPC cancellation.

## Resource List (v1)

Resources are read-only snapshots. Query parameters mirror tool defaults.

- `wa://panes` — Current pane registry (same schema as `wa.state`)
- `wa://events` — Event feed (same schema as `wa.events`)
- `wa://accounts` — Account status (same schema as `wa.accounts`)
- `wa://workflows` — Available workflows
- `wa://rules` — Pattern rules (same schema as `wa.rules_list`)
- `wa://reservations` — Active reservations (same schema as `wa.reservations`)

## Error Codes (stable)

All MCP errors use stable codes prefixed with `FT-MCP-`:

| Error code | Meaning | Robot equivalent |
|------------|---------|------------------|
| `FT-MCP-0001` | Invalid arguments | `robot.invalid_args` |
| `FT-MCP-0003` | Config error | `robot.config_error` |
| `FT-MCP-0004` | Backend bridge CLI error (current: WezTerm) | `robot.wezterm_error` |
| `FT-MCP-0005` | Storage error | `robot.storage_error` |
| `FT-MCP-0006` | Policy denied or approval required | `robot.policy_denied`, `robot.require_approval` |
| `FT-MCP-0007` | Pane not found | `robot.pane_not_found` |
| `FT-MCP-0008` | Workflow error | Workflow-related robot errors, for example `robot.workflow_error`, `robot.workflow_aborted`, `robot.workflow_not_found`, `robot.mission_error`, `robot.tx_error` |
| `FT-MCP-0009` | Timeout | `robot.timeout` |
| `FT-MCP-0010` | Not implemented | Unsupported/unavailable robot errors, for example `robot.unsupported`, `robot.feature_not_available` |
| `FT-MCP-0011` | Search query lint/FTS syntax error | `robot.fts_query_error` |
| `FT-MCP-0012` | Reservation conflict | `robot.reservation_conflict` |
| `FT-MCP-0013` | CAAM/CAUT account backend error | `robot.caut_error` |
| `FT-MCP-0014` | CASS integration error | `robot.cass_error` |
| `FT-MCP-0015` | Remote pane text unavailable | `robot.remote_text_unavailable` |
| `FT-MCP-0016` | Durable event cursor discontinuity (retention, epoch, scope, or authority mismatch) | `robot.cursor_discontinuity` |
| `FT-MCP-0017` | Mutation outcome is indeterminate; reconcile state and do not retry automatically | `robot.wezterm_mutation_indeterminate` or the corresponding storage indeterminate class |
| `FT-MCP-9000` | Internal MCP/server error with redacted details | Internal runtime, IO, JSON, setup, cancellation, or other unclassified failures |

## Safety & Policy

Any tool that causes side effects MUST pass the PolicyEngine, including: <!-- MCP-V1-003 -->
- `wa.send`
- `wa.workflow_run` / `wa.workflow_abort`
- `wa.approve`
- `wa.reserve` / `wa.release`
- `wa.accounts_refresh` (if it triggers external calls)

Resources are read-only and MUST not cause side effects. <!-- MCP-V1-004 -->

Policy + redaction also apply to read/query tools:
- `wa.get_text`
- `wa.search`

These surfaces may return `FT-MCP-0006` when policy denies access or requires approval, and returned text fields are redacted.

## Parity & Schema Contract

MCP and Robot Mode share envelope conventions, stable error semantics, and data
schemas only where a tool explicitly claims that parity. Capability differences
are part of the contract: for example, `wa.events` is a non-resumable snapshot
while `ft robot events` also offers scoped cursor modes, and `wa.await_event`
supports rule conditions plus optional claim leasing while the CLI additionally
supports storage-derived quiescence. Every MCP error still maps to a stable code
from the catalog above; callers must follow the per-tool parameter and response
notes rather than assuming 1:1 command-line equivalence.
