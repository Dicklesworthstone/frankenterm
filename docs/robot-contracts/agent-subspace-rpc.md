# Agent Subspace RPC Contract

`wa://subspace/rpc` is the terminal-bypass coordination contract for
agent-to-agent messages that must not depend on terminal scrollback. It is not a
replacement for Agent Mail. Agent Mail remains the durable human-auditable
mailbox and archive; Agent Subspace RPC is the compact control-plane envelope
for request/receipt style coordination between active agents.

## Decision

Extending Agent Mail alone is not sufficient for this surface. Mail is optimized
for asynchronous delivery, inbox/outbox history, acknowledgements, and
cross-agent coordination records. Terminal-bypass RPC needs a stricter wire
contract: every request has a serialized payload digest, policy receipt,
redaction receipt, audit receipt, and delivery receipt that proves the payload
was not delivered through the terminal render buffer.

The bridge between the two is allowed in either direction:

- RPC receipts may be archived to Agent Mail for human review.
- Agent Mail outage handling may queue a later RPC retry.
- Neither path may skip the RPC policy, redaction, audit, or delivery receipts.

## Payload

The first contract version is `schema_version = 1` and uses JSON serialization.
The stable data shape is `AgentSubspaceRpcData` in
`crates/frankenterm-core/src/robot_types.rs`, with the supplemental JSON Schema
row at `docs/json-schema/wa-robot-api-surface-data-schemas.json`.

Required top-level fields:

| Field | Requirement |
| --- | --- |
| `route` | Always `wa://subspace/rpc`. |
| `rpc_id` | Non-empty request id. |
| `idempotency_key` | Non-empty replay key. |
| `sender_agent` / `recipient_agent` | Non-empty agent identifiers. |
| `serialization` | `json` in v1. |
| `payload` | JSON value whose serialized bytes are bounded. |
| `payload_bytes` | Exact byte count of the stable JSON serialization. |
| `payload_sha256` | SHA-256 digest of the stable JSON serialization. |
| `policy` | Decision plus non-empty policy id. |
| `redaction` | Redaction state, sensitivity tier, and redacted-field count. |
| `audit` | Non-empty audit id plus record timestamp. |
| `delivery` | Terminal-bypass delivery receipt. |

The initial cap is 64 KiB per payload. Larger content must be referenced by an
artifact URI or archived through a durable channel instead of embedded in the
RPC payload.

## Delivery Invariant

`delivery.mode` must be `terminal_bypass`, and
`delivery.terminal_render_buffer` must be `false`.

Any implementation that writes the payload into a pane, scrollback buffer, or
terminal-rendered text stream violates this contract even if the JSON shape is
otherwise valid.

## Proof Lane

Focused proof for this contract:

```bash
RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-7h5da-11-11-agent-subspace cargo test -p frankenterm-core --lib agent_subspace_rpc -- --nocapture
```

That lane exercises the typed contract validation and the
`ApiSurface::AgentSubspaceRpc` matrix row. Wider CLI or MCP routing is a
follow-on implementation step; it must preserve this contract rather than
inventing a second payload shape.
