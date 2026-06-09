# Agent Mail Outage Spool — Epic Completion & Convergence (ft-dezx8 / ft-dezx8.6)

> Closeout for the **durable Agent Mail fallback outbox + replay receipts** epic
> (`ft-dezx8`). Records the completion matrix, the attestation decision, the
> operator closeout checklist, the residual-risk surface, and the convergence
> proof. Authored under `ft-dezx8.6` (W6 — convergence proof + release-attestation
> wiring).

## Convergence state

| Bead | Title | Status |
|---|---|---|
| `ft-dezx8` | Durable Agent Mail fallback outbox and replay receipts (epic) | open → closeable (this doc) |
| `ft-dezx8.1` | Receipt/entry schema + fixture corpus | CLOSED |
| `ft-dezx8.2` | Safe writer adapter (queue without delivery claims) | CLOSED |
| `ft-dezx8.3` | Exact-once replay runner + dry-run verifier | CLOSED |
| `ft-dezx8.4` | Robot + operator surfaces for queued coordination | CLOSED |
| `ft-dezx8.5` | Golden outage corpus + no-mock replay harness | CLOSED |
| `ft-dezx8.6` | Convergence proof + release-attestation wiring | in_progress (this doc) |

All implementation children are closed; `br dep cycles --json` = `count 0`,
`active_ft 0`.

## Completion matrix

Each load-bearing claim of the outage-spool epic mapped to code, docs, tests,
retained artifacts, and proof state.

| Component / claim | Code | Docs / schema | Tests | Retained artifact | Proof state |
|---|---|---|---|---|---|
| **Entry schema** — a queued outage entry has a typed shape with a bounded failure class | `crates/frankenterm-core/src/agent_mail_outbox.rs` (`FailureClass`, `OutboxState`, entry types) | `docs/json-schema/ft-agent-mail-outbox-entry.json` | e2e contract (per-fixture `fixture_checked`) | `fixtures/agent-mail-outage-spool/valid/*.json` (10) | static verifier GREEN |
| **Surface projection** — read-only summary over queued/replayable/failed/delivered state | `agent_mail_outbox.rs` (`AgentMailOutboxSurface`, `load_agent_mail_outbox_surface`, `build_agent_mail_outbox_surface`, `OutboxDeliveryClaim`) | contract id `ft.agent_mail_outbox_surface.v1`; `docs/security/read-path-redaction-matrix.md` (BCC redaction) | e2e `surface_summary` event | `fixtures/.../expected/verify.v1.jsonl` | static verifier GREEN |
| **Failure taxonomy** — distinct classes incl. `timeout`, `reservation_conflict`, `contact_permission_blocked`, `api_unreachable`, `api_error`, `database_recovery_notice`, `agent_mail_unavailable` | `agent_mail_outbox.rs` (`classify_agent_mail_failure`, `FailureClass::*`) | `ft-agent-mail-outbox-entry.json` failure-class enum | e2e `contract_summary.failure_classes` | corpus fixtures (one per class) | static verifier GREEN |
| **Safe writer adapter** — queue without claiming delivery | `ft-dezx8.2` writer adapter | — | exact-once dry-run verifier (`ft-dezx8.3`) | `replay_dry_run_ok` golden case | static verifier GREEN |
| **Exact-once replay** — dry-run + replay; delivery only on a real receipt | `ft-dezx8.3` runner | — | e2e replay states (`replayed`, `replay_dry_run_ok`, `replay_failed`) | `replayed-send`, `ack-required-message`, `stale-owner-handoff` fixtures | static verifier GREEN |
| **Robot surface** — `ft robot agent-mail-outbox` | `crates/frankenterm/src/main.rs` (`RobotCommands::AgentMailOutbox`) | `README.md` (robot status table + usage), `docs/cli-reference.md` | CLI parse tests in `main.rs` | — | source-landed; Rust proof deferred (RCH) |
| **MCP surface** — `wa://agent-mail/outbox` resource | `crates/frankenterm-core/src/mcp_resources.rs` (`WaAgentMailOutboxResource`), `mcp.rs`, `mcp_bridge.rs` | `docs/mcp-api-spec.md` | inline mcp tests | — | source-landed; Rust proof deferred (RCH) |
| **Operator / Beads fallback** — when mail is red, coordinate via Beads | `scripts/swarm-tick.sh --agent-mail-fallback` | `AGENTS.md` (Agent Mail process-protection + fallback) | e2e `beads-fallback-closeout` fixture (`superseded`) | fixture | static verifier GREEN |
| **No-mock golden corpus** | — | `fixtures/agent-mail-outage-spool/manifest.json` | `tests/e2e/test_agent_mail_outage_spool_contract.sh` | `manifest.json` + `expected/verify.v1.jsonl` | static verifier GREEN |
| **Safety: no service repair** — never restart/repair/kill Agent Mail | `agent_mail_outbox.rs` (read-only paths only) | `AGENTS.md` (DO NOT TOUCH) | — | — | by construction (read-only) |
| **Safety: delivery never overclaimed** — `delivery_unclaimed` until a replay receipt exists | `agent_mail_outbox.rs` (`OutboxDeliveryClaim`) | this doc (residual risk) | e2e `surface_summary.delivery_unclaimed=9` | `expected/verify.v1.jsonl` | static verifier GREEN |

## Attestation decision (release-claim wiring)

**No durable-delivery attestation slot is added, by design.** The shipped
surface is explicitly **read-only outage review / replay planning, not live
delivery proof** — the README states queued entries, dry-run-ok rows, and
fixture replay logs are *not* Agent Mail delivery proof until a replay receipt
records a delivered message id. Wiring a `security/*durable-delivery*`
attestation slot would overclaim exactly the property the surface refuses to
assert.

The retained, release-citable proof for this epic is therefore the **static
contract verifier**, not a signed delivery claim:

- Producer: `tests/e2e/test_agent_mail_outage_spool_contract.sh`
- Golden: `fixtures/agent-mail-outage-spool/expected/verify.v1.jsonl`
  (`contract_id: ft.agent_mail_outbox_surface.v1`, 10 fixtures)

This mirrors how `proofs/deferred-proof-replay` attests *its* static verifier
rather than live RCH replays. If a future release wants the outage-spool surface
to appear in the signed bundle, the honest slot is a `proofs/agent-mail-outage-spool`
entry pointing at the `expected/verify.v1.jsonl` golden (attesting the contract,
**not** delivery) — proposed here, intentionally not wired this round to avoid a
manifest claim stronger than the code.

## Operator closeout checklist

When closing outage-spool work after an Agent Mail outage, record:

1. **Failure reason** — the exact `FailureClass` observed (e.g. `api_unreachable`
   after the single retry, `reservation_conflict`, `timeout`) and the
   `am`/`swarm-tick` reason codes (`agent_mail.unavailable_after_retry`,
   `fallback.beads_only`).
2. **Recovery reason** — how the service came back (listener returned on
   `127.0.0.1:8765`); whether you re-registered (`am macros start-session
   --agent-name <name>`) and re-checked the inbox.
3. **Replay receipt IDs** — for any entry that moved to `replayed`, the delivered
   message id from the replay receipt (delivery stays `unclaimed` without one).
4. **Beads graph state** — `br dep cycles --json` count, and the in-progress /
   handed-off beads coordinated via Beads while mail was red.
5. **RCH proof details** — for any Rust code touched: the exact
   `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec --
   env CARGO_TARGET_DIR=… cargo …` command and its admission/topology-preflight
   outcome. A `topology_preflight_failed` block is `wait_rch`, not local-proof
   permission.
6. **Never** run `am service restart/stop`, `am doctor fix/repair/reconstruct`,
   or kill any `am`/`mcp-agent-mail` process (AGENTS.md, hard rule).

## Residual risk — messages that cannot be cleanly replayed

A queued entry is **not** guaranteed replayable; the surface marks delivery
`unclaimed` and these classes may never deliver as originally intended:

- **Contact policy changed while mail was down** — the recipient revoked/altered
  contact approval; a queued `send_message` would now be `contact_permission_blocked`.
- **Recipient identity changed** — the target agent re-registered under a new id
  / session; the original `to_agent` no longer resolves.
- **Attachments / payload references expired** — a queued message referencing a
  reservation (`reservation_intent`) or file that was since released, edited, or
  whose ownership moved (`stale_owner_handoff`, `database_recovery_notice`).
- **Operator supersession** — the work was already handed off or closed via the
  Beads fallback (`beads_closeout_comment` → `superseded`); replaying the
  original message would duplicate or contradict the live state.

For all of these the correct behavior is to surface the entry for human/robot
review and leave delivery `unclaimed` — never to silently "deliver" a stale
message. This is the property the no-durable-delivery attestation decision
protects.

## Convergence proof (retained)

```
children: ft-dezx8.1..5 = CLOSED (this doc, convergence table)
e2e:  bash tests/e2e/test_agent_mail_outage_spool_contract.sh
      => "agent mail outage spool contract: static verifier passed (10 fixtures)"
      contract_summary.fixtures=10; surface_summary.contract_id=ft.agent_mail_outbox_surface.v1
      surface_summary.delivery_unclaimed=9 (delivery never overclaimed)
cycles: br dep cycles --json => {count: 0, active_ft: 0}
rch:  not applicable — this closeout bead touches no Rust; Rust surfaces
      (robot/MCP) are source-landed with Rust proof deferred behind the RCH
      remote-topology-preflight infra block.
```
