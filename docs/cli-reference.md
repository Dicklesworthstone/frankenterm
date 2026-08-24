# CLI Reference

This reference is a concise, accurate snapshot of the current command surface.
Use it as the command-truth companion to:

- `docs/ft-xbnl0-verification-contract.md`
- `docs/ft-xbnl0-3-6-supported-path-truth-sweep.md`
- `docs/ft-xbnl0-4-6-completion-evidence.md`
- `docs/ft-xbnl0-5-7-completion-evidence.md`

Commands marked as feature-gated require building with the corresponding feature.

## Human CLI (implemented)

### Watcher and status

```bash
ft watch [--foreground] [--auto-handle] [--poll-interval <ms>]
ft stop [--force] [--timeout <secs>]
ft status [--health]
ft list [--json]
ft show <pane_id> [--output]
ft get-text <pane_id> [--tail <n>] [--escapes]
```

Distributed mode notes:
- With `--features distributed` and `[distributed].enabled = true`, `ft watch` also acts as the aggregator listener.
- Remote agents connect with `ft distributed agent --connect <host:port> --agent-id <name>`.
- Aggregated remote panes are persisted into the same DB and surface through `ft status`, `ft search`, `ft robot state`, MCP `wa.state`, and `wa://panes`.

### Distributed mode

```bash
ft distributed agent [--connect <host:port>] [--connect-addr <host:port>] [--agent-id <name>]
```

Behavior notes:
- `--connect` overrides both `--connect-addr` and the config default for a single run.
- `--connect-addr` separates the agent's upstream target from the server-side `distributed.bind_addr`.
- Live readback for distributed panes is not available through `get-text`; use `ft search` or `ft robot search` against persisted output instead.

### Search and events

```bash
ft search "<fts query>" [--pane <id>] [--limit <n>] [--since <epoch_ms>] [--until <epoch_ms>] [--mode <lexical|semantic|hybrid>]
ft events [--unhandled] [--pane-id <id>] [--rule-id <id>] [--event-type <type>]
ft events annotate <event_id> --note "<text>" [--by <actor>]
ft events annotate <event_id> --clear [--by <actor>]
ft events triage <event_id> --state <state> [--by <actor>]
ft events triage <event_id> --clear [--by <actor>]
ft events label <event_id> --add <label> [--by <actor>]
ft events label <event_id> --remove <label>
ft events label <event_id> --list
ft triage [--severity <error|warning|info>] [--only <section>] [--details]
```

Command-truth note:
- `ft search` is the canonical top-level search surface in current root help.
- `ft query` is not advertised as a first-class root command; when accepted, it resolves to the `ft search` help surface.

Mode notes:
- `lexical` uses FTS5/BM25 ranking.
- `semantic` uses embedding-backed retrieval with fused ranking score output.
- `hybrid` fuses lexical + semantic lanes with deterministic rank fusion.

### Actions, approvals, and audit

```bash
ft send <pane_id> "<text>" [--dry-run] [--wait-for "<pat>"] [--timeout-secs <n>]
ft send <pane_id> "<text>" --no-paste --no-newline
ft prepare send --pane-id <id> "<text>"
ft prepare workflow run <name> --pane-id <id>
ft commit <plan_id> [--text "<text>"] [--text-file <path>] [--approval-code <code>]
ft approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
ft audit [--limit <n>] [--pane <id>] [--action <kind>] [--decision <allow|deny|require_approval>]
ft history [--limit <n>] [--pane <id>] [--actor <kind>] [--workflow <id>] [--undoable]
ft history [--action <kind>] [--decision <allow|deny|require_approval>] [--result <status>] [--since <time>] [--until <time>]
ft history --export <json|csv>
ft undo --list [--limit <n>]
ft undo <action_id> [--yes]
ft undo --all-in-workflow <id> [--yes]
```

See `docs/approvals.md` for the prepare/commit mental model and troubleshooting.

### Reservations

```bash
ft reserve <pane_id> [--ttl <secs>] [--owner-kind <workflow|agent|manual>] [--owner-id <id>]
ft reservations [--json]
```

### Workflows

```bash
ft workflow list
ft workflow run <name> --pane <id> [--dry-run]
ft workflow status <execution_id> [-v|-vv]
```

### Mission control

```bash
ft mission plan [--mission-file <path>] [--include-dispatch-contracts] [-f <plain|json>]
ft mission run [--mission-file <path>] [-f <plain|json>]
ft mission status [--mission-file <path>] [-f <plain|json>]
ft mission explain [--mission-file <path>] [--assignment-id <id>] [-f <plain|json>]
ft mission pause [--mission-file <path>] [--reason <text>] [-f <plain|json>]
ft mission resume [--mission-file <path>] [-f <plain|json>]
ft mission abort [--mission-file <path>] [--reason <text>] [-f <plain|json>]
ft steer plan --objective <text> --scenario <clean-ready|dirty-overlap|rch-blocked|approval-required|capacity-red> [--workspace-id <id>] [--ttl-ms <n>] [-f <plain|json>]
ft steer run --receipt <steer:id> [--mission-file <path>] [--plan-hash <hash>] [-f <plain|json>]
```

Transaction-contract control is currently surfaced under robot mode as
`ft robot tx plan|run|rollback|show`; the top-level human CLI does not expose
`ft tx` today.

### Proof queue and deferred replay

```bash
ft proof queue [--queue-file <path>] [--bead <id>] [--package <crate>] [--kind <test|check|clippy|fmt|schema|fuzz|replay|attestation>] [--source-hash <hash>] [--expected-artifact <path>] [--attestation-slot <slot>] [--redaction-policy <standard|strict>] [--admission-state <state>] [-f <plain|json|toon>] -- <remote-required-rch-command>
ft proof status [--queue-file <path>] [--source-hash <hash>] [--admission-state <state>] [-f <plain|json|toon>]
ft proof replay [--queue-file <path>] [--source-hash <hash>] [--admission-state <state>] [--artifact-dir <dir>] [--dry-run] [-f <plain|json|toon>]
ft proof attach <intent_id> [--queue-file <path>] --receipt <path> [-f <plain|json|toon>]
```

`ft proof` is a fail-closed proof-debt surface. `queue` stores the exact
remote-required RCH command and source hash, `status` classifies stale/deferred
or replayable intents, `replay --dry-run` explains the selected candidate, and
live replay only executes when admission is explicitly `admitted` and the source
hash still matches. It never substitutes local Cargo for a remote proof.

### Rules

```bash
ft rules list [--agent-type <codex|claude_code|gemini|wezterm>]
ft rules test "<text>"
ft rules show <rule_id>
```

For explain-match traces and how to interpret robot `--trace` output, see
`docs/explain-match.md`.

### Diagnostics and bundles

```bash
ft doctor
ft doctor --json
ft diag bundle [--output <dir>] [--events <n>] [--audit <n>] [--workflows <n>]
ft reproduce export [--kind <crash|manual>] [--out <dir>] [--format <text|json>]
ft reproduce replay <bundle_dir> [--mode <full|policy|rules>]
```

### Session persistence and forensic export

```bash
ft session dump [--output <path>] [--allow-partial] [-f <auto|plain|json|toon>]
ft session verify-dump <path> [-f <auto|plain|json|toon>]
ft session list-durable [--max-entries <n>] [-f <auto|plain|json|toon>]
ft session export-durable <32-hex-pane-id> [--output <path>] [--max-rows <n>] [--max-total-bytes <n>] [--max-physical-bytes <n>] [-f <auto|plain|json|toon>]
ft session list-orphans [-f <auto|plain|json|toon>]
ft session recover <64-hex-pane-uuid> [--output <path>] [--allow-partial] [-f <auto|plain|json|toon>]
ft session discard <64-hex-pane-uuid> --force [-f <auto|plain|json|toon>]
```

`recover` is export-only and never writes archived output into a live PTY. It
rejects an incomplete source or skipped record before creating an artifact
unless `--allow-partial` is explicit; an opted-in salvage reports `complete:
false` with the bounded stop reason. `discard` removes only the still-leased,
identity-revalidated data leaf, synchronizes its pinned parent directory, and
retains the lock inode to preserve one flock authority.

### Web server and streaming API

Requires `--features web`.

```bash
ft web [--port <n>]
```

Default bind is `127.0.0.1:8000`. The web server exposes:

```bash
GET /health
GET /panes
GET /events
GET /search
GET /bookmarks
GET /ruleset-profile
GET /saved-searches
GET /stream/events
GET /stream/deltas
```

Streaming endpoints return `text/event-stream` with schema `ft.stream.v1`.

`/stream/events` query parameters:
- `channel=<all|deltas|detections|signals>` to select the live bus lane
- `pane_id=<id>` to filter to one pane
- `max_hz=<n>` to cap event delivery rate

`/stream/deltas` query parameters:
- `pane_id=<id>` to stream one pane's captured output
- `max_hz=<n>` to cap frame rate

Examples:

```bash
ft web
curl -N http://127.0.0.1:8000/stream/events
curl -N "http://127.0.0.1:8000/stream/events?channel=detections&pane_id=7&max_hz=25"
curl -N "http://127.0.0.1:8000/stream/deltas?pane_id=7&max_hz=50"
```

Behavior notes:
- Streams emit keepalive comments while idle.
- Secret material is redacted before SSE frames are emitted.
- Bounded channels and `max_hz` provide fan-out backpressure control.

### Setup and config

```bash
ft setup [--list-hosts] [--dry-run] [--apply]
ft setup local
ft setup remote <host> [--yes] [--install-ft \
  (--ft-version <release-tag> | \
   --ft-path <target-ft> --mux-server-path <target-mux-server>) \
  --transaction-id <32-lowercase-hex>]
ft setup config
ft setup patch [--remove]
ft setup shell [--remove] [--shell <bash|zsh|fish>]
ft setup font [--force] [--dir <path>]

ft config init [--force]
ft config validate [--strict]
ft config show [--effective] [--json]
ft config set <key> <value> [--dry-run]
ft config export [-o <path>] [--json]
ft config import <path> [--dry-run] [--replace] [--yes]
```

An applied remote `--install-ft` requires a caller-chosen stable transaction ID;
reuse that exact ID after timeout or lost acknowledgement. The current command
publishes a verified immutable pending process-family generation but does not
activate it, rewrite/start the mux service, or stop a live mux.

### Data management

```bash
ft db migrate [--status] [--dry-run]
ft db check [-f <auto|plain|json>]
ft db repair [--dry-run] [--yes] [--no-backup]

ft backup export [-o <dir>] [--sql-dump]
ft backup import <path> [--dry-run] [--verify]

ft export <segments|events|audit|workflows|sessions> [--pane-id <id>] [--since <epoch_ms>]
```

### Learning and auth

```bash
ft learn [<track>] [--status] [--achievements] [--reset] [--complete] [--skip]
ft auth test <service> [--account <name>] [--timeout-secs <1..=1800>]
ft auth status <service> [--account <name>]
ft auth status --all
ft auth bootstrap <service> [--account <name>] [--timeout-secs <1..=1800>]
```

Notes:
- `ft auth` requires the `browser` feature to enable Playwright-based flows.
- `ft auth test` validates bounded local runtime/profile/storage-state evidence;
  it does not launch a browser or prove that a remote service still accepts the
  persisted credentials.
- `ft auth status --all` uses bounded, no-symlink profile discovery and reports
  whether the scan was incomplete or truncated.
- `ft auth bootstrap` is the explicitly interactive command and opens a visible
  browser window.

## Feature-gated commands

```bash
ft tui          # requires --features tui
ft mcp serve    # requires --features mcp
ft web          # requires --features web; serves HTTP + SSE `/stream/*`
ft sync         # requires --features sync
```

## Robot mode (stable JSON/TOON)

Robot mode uses stable machine contracts. MCP shares the common envelope and
documented data schemas where the two surfaces expose the same capability;
surface-specific differences are explicit rather than implied parity.

```bash
ft robot help
ft robot quick-start
ft robot state [--include-text] [--tail <n>] [--escapes]
ft robot get-text <pane_id>|--panes <id,id,...>|--all [--tail <n>] [--escapes]
ft robot dom zones|last-command|output-of|exit-code <pane_id> [--command-index <n>]
ft robot send <pane_id> "<text>" [--dry-run] [--verify-submit] [--submit-level <write|composer|submitted|working>] [--wait-for "<pat>"] [--timeout-secs <n>]
ft robot wait-for <pane_id> "<pat>" [--timeout-secs <n>] [--regex]
ft robot search "<fts query>" [--pane <id>] [--since <epoch_ms>] [--until <epoch_ms>] [--limit <n>] [--snippets[=<true|false>]] [--mode <lexical|semantic|hybrid>]
ft robot search-explain "<fts query>" [--pane <id>]
ft robot search-index stats
ft robot search-index reindex [--pane <id>] [--batch-size <n>] [--since <epoch_ms>] [--until <epoch_ms>]
ft robot cass status
ft robot cass search "<query>" [--agent <kind>] [--workspace <substr>] [--days <n>] [--limit <n>]
ft robot cass view <source_path> <line_number> [--context-lines <n>]
ft robot events [--unhandled] [--pane <id>] [--rule-id <id>] [--event-type <type>] [--triage-state <state>] [--label <label>] [--since <nonnegative-epoch-ms>] [--limit <1..4096>] [--cursor <id> --cursor-epoch <32-lowercase-hex> --cursor-scope <64-lowercase-hex>] [--replay-limit <1..4096>] [--start-at-tail]
ft robot events annotate <event_id> --note "<text>" [--by <actor>]
ft robot events annotate <event_id> --clear [--by <actor>]
ft robot events triage <event_id> --state <state> [--by <actor>]
ft robot events triage <event_id> --clear [--by <actor>]
ft robot events label <event_id> --add <label> [--by <actor>]
ft robot events label <event_id> --remove <label>
ft robot events label <event_id> --list
ft robot watch-events [--follow] [--severity <critical|error|warning|info>] [--rule-id <glob>] [--pane <id>] [--unhandled] [--claim] [--cursor <id> --cursor-epoch <32-lowercase-hex> --cursor-scope <64-lowercase-hex>] [--limit <n>] [--heartbeat-interval-ms <ms>] [--poll-interval-ms <ms>] [--max-hz <n>]
ft robot await [--any 'rule:<glob>'|'quiescence:<pane>[:<idle_ms>]'] [--all 'rule:<glob>'|'quiescence:<pane>[:<idle_ms>]'] [--timeout-secs <n>] [--poll-interval-ms <ms>] [--checkpoint-only] [--cursor <id> --cursor-epoch <32-lowercase-hex> --cursor-scope <64-lowercase-hex>]

ft robot workflow list
ft robot workflow run <name> <pane_id> [--force] [--dry-run]
ft robot workflow status [<execution_id>] [--pane <id>] [--active] [--verbose]
ft robot workflow abort <execution_id> [--reason "..."] [--force]

ft robot rules list [--pack <name>] [--agent-type <type>]
ft robot rules test "<text>" [--trace] [--pack <name>]
ft robot rules show <rule_id>
ft robot rules lint [--pack <name>] [--fixtures] [--strict]

ft robot agents list|running|detect|configure
ft robot accounts list [--pick]
ft robot accounts refresh
ft robot reservations list|reserve|release
ft robot mission state|decisions
ft robot tx plan|run|rollback|show
ft robot health
ft robot proof status [--queue-file <path>] [--source-hash <hash>] [--admission-state <state>]
ft robot approve <code> [--pane <id>] [--fingerprint <hash>] [--dry-run]
ft robot why <code>
ft robot agent-mail-outbox [--manifest <path>] [--entry <path> ...]

ft robot checkpoint save|list|show|delete|rollback
ft robot context status|rotate|history
ft robot work claim|release|complete|list|ready|assign
ft robot fleet status|scale|rebalance|agents
ft robot profile list|show|apply|validate
```

`ft robot events` has three deliberately distinct modes. With no cursor options
it preserves the legacy newest-first snapshot and is not resumable. With
`--start-at-tail` it returns no historical events and establishes an atomic
durable-tail checkpoint in `next_cursor`, `next_cursor_epoch`, and
`next_cursor_scope`. Supplying that complete triple enters resumable
ascending-ID mode; `--replay-limit` is valid only in this mode. The scope is a
SHA-256 fingerprint of every membership filter, including `--since`, so a
token cannot be reused after changing filters. `--since` is a nonnegative Unix
epoch timestamp in milliseconds.

`ft robot watch-events` and `ft robot await` are fixed compact-NDJSON streams
whose closed record union is `docs/json-schema/wa-robot-event-stream-record.json`;
their success, control, and error records do not switch encoding when the
global `--format` setting is JSON-pretty or TOON. In particular,
`--format toon` does not apply to these two streaming commands. Omitting the
cursor triple does not replay existing rows. The command first acquires the
current durable tail atomically and emits a committed `cursor_checkpoint`; only
events created after that successful acquisition are eligible. Except in
`--checkpoint-only` mode, a fresh await therefore emits at least two records:
the baseline checkpoint and one terminal `await_result`. Resume by copying the
complete cursor/epoch/scope triple. Await
conditions may combine rule globs with storage-derived pane quiescence; an
omitted quiescence idle threshold is canonicalized to the configured default
and is part of the scope fingerprint. A missing pane produces a terminal
`robot.pane_not_found` record rather than being mistaken for a pane with no
captured output. `await --checkpoint-only` emits that exact condition-scoped
tail checkpoint and exits, which lets a caller bootstrap a token without a
timeout, a matching event, or terminating a long-lived child process. Each of
`--any` and `--all` accepts at most 16 conditions, and each condition is bounded
to 256 UTF-8 bytes.

Terminal error records preserve a resume token only as one complete canonical
cursor/epoch/scope triple. If no complete canonical token is available, all
three fields are `null`; partial or malformed caller input is never reflected
as apparent cursor authority.

Rule matches inside a composite await are request-local latches. The published
resume cursor advances across irrelevant rows, then holds immediately before
the first rule occurrence needed by a still-incomplete composite. A timeout or
recoverable error returns that held token so a later request replays the hidden
partial match. A successful await commits only through the exact event that
completed the composite, leaving later occurrences in the same storage page
available to the next wait.

`ft robot watch-events --claim` is flush-before-handle and at-least-once. It
atomically leases each persisted event, emits and flushes a pending record whose
public `cursor` is still the last committed checkpoint, whose
`candidate_cursor` identifies the leased event, and whose `pending_finalize`
field is true. It then token-CAS marks the row handled and emits a committed
`cursor_checkpoint`. A known write or flush failure attempts an immediate
token-CAS lease release. If release cannot reach storage, lease expiry remains
the recovery path; a crash also leaves the event eligible for redelivery after
expiry. The durable cursor advances only after successful finalization or
exact evidence that another path already handled the row. In one-shot mode,
ambiguous finalization emits `claim_deferred` with the unchanged committed
triple; follow mode retains that cursor and retries. A missing row advances no
cursor without authoritative retention evidence. Finalization races can
therefore duplicate but cannot silently skip an event. Live-only
`pane_discovered` and `pane_disappeared` notifications have no durable row and
are not claimable.

Every durable event cursor is a three-part token: a nonnegative event ID, a
128-bit lowercase-hex retention epoch, and a 256-bit lowercase-hex scope
fingerprint. Resume commands require all three values. Cursor pages advance
through the exact examined high-water mark even when filters omit rows, while
any pruned, legacy-ambiguous, stale-epoch, wrong-scope, or ahead-of-authority
cursor fails closed with a typed terminal discontinuity. FrankenTerm never
silently re-baselines a durable consumer; intentionally starting fresh uses
`events --start-at-tail` or omits the complete triple for Watch/Await.

In follow mode, a persisted detection received from the live IPC EventBus is a
low-latency wakeup, not an ordering or payload authority. FrankenTerm re-enters
the SQLite cursor drain and emits the exact stored rows in increasing event-ID
order; this prevents out-of-order EventBus publication or dedupe conflicts from
advancing the resumable cursor past an unseen row. Live-only pane-lifecycle
notifications remain direct, best-effort records with `id: null`; the public
relay attaches the follower's current committed cursor triple. The private IPC
subscription requests a heartbeat no slower than the configured durable DB
poll cadence, and each private heartbeat returns the follower to the SQLite
drain. `--heartbeat-interval-ms 0` disables user-visible heartbeat records,
not this private durability wakeup.

`--limit` is the per-poll batch size and must be in `1..=1000`; values outside
that hard bound fail before FrankenTerm opens watch-event storage.

`ft robot send` keeps the default fast path unless the caller asks for delivery
proof. `--verify-submit` returns a submitted-level `submit` receipt; `--submit-level`
selects `write`, `composer`, `submitted`, or `working`. The receipt records the
submission state, requested guarantee level, `guarantee_met`, verification
polls, elapsed time, evidence rule IDs, and idempotency key persisted as the
audit `correlation_id`; replay it with
`ft audit --correlation-id <idempotency_key>`.

`ft robot health` includes an `active_agents` snapshot for operator
convergence polling. The snapshot is bounded and evidence-linked; unavailable
joins such as Beads assignments, recent commits, or proof lanes are reported
explicitly under `active_agent_sources` instead of being inferred.

`ft robot agent-mail-outbox` reads retained Agent Mail outage-spool fixtures and
queued-entry files. It is a read-only contract/replay surface: queued or
`replay_dry_run_ok` rows are not Agent Mail delivery proof, and the command does
not repair, restart, or mutate the shared Agent Mail service.

`ft robot proof status` is the read-only robot projection of the deferred proof
queue. It returns the same status payload as `ft proof status --format json`,
including source freshness, replay eligibility, RCH admission blockers, attempt
counts, and attached-receipt counts.

Examples:
- `ft robot search "compilation failed" --mode lexical`
- `ft robot search "compilation failed" --mode semantic`
- `ft robot search "compilation failed" --mode hybrid`

Notes:
- `ft robot help` emits the machine-readable command inventory by default; use `ft robot --format toon help` for the compact TOON form.
- Distributed panes appear in `state`, `search`, and related persisted-data surfaces, but live `get-text` is intentionally unavailable for them.
- `ft robot dom` is the **semantic pane API** — despite the verb name it returns a **flat list** of live OSC 133 semantic zones, **not** a DOM tree (no nesting, parent/child, or root; the envelope has no `children`). Panes without OSC 133 prompt/input markers return `semantic_data_unavailable=true` instead of guessed command boundaries. Full contract: [`docs/robot-contracts/semantic-pane-api.md`](robot-contracts/semantic-pane-api.md).
- Some NTM-aligned robot families are specialized machine surfaces; they are listed here because they ship, even when most humans stay on the higher-level human CLI.

Policy/redaction:
- `ft get-text`, `ft search`, `ft robot get-text`, `ft robot dom`, and `ft robot search` are policy-gated read/search surfaces.
- Returned text/snippets are passed through the standard secret redactor before output.
- Redaction applies to echoed query/content fields as well (`query`, `snippet`, `content`) for search responses.
- Policy denials return `robot.policy_denied`; approval-required paths return `robot.require_approval` with approval guidance.

## MCP reference

MCP tools mirror robot mode. See `docs/mcp-api-spec.md` and `docs/json-schema/` for details.

Tools (tool IDs currently still use the `wa.*` prefix):
- wa.state
- wa.get_text
- wa.send
- wa.wait_for
- wa.search
- wa.events
- wa.events_annotate
- wa.events_triage
- wa.events_label
- wa.workflow_run
- wa.accounts
- wa.accounts_refresh
- wa.rules_list
- wa.rules_test
- wa.rules_show
- wa.rules_lint
- wa.reserve
- wa.release
- wa.reservations
- wa.approve
- wa.why
- wa.workflow_list
- wa.workflow_status
- wa.workflow_abort

Resources (resource URIs currently still use the `wa://` scheme):
- wa://panes
- wa://events
- wa://accounts
- wa://workflows
- wa://rules
- wa://reservations
