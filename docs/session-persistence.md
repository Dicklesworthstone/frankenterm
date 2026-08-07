# Session Persistence (Snapshots)

ft’s session persistence system captures terminal-backend mux evidence (current
bridge: WezTerm) into SQLite snapshots so you can:

- Inspect the bounded metadata needed to plan a manual reconstruction after an unclean shutdown
- Preserve evidence before an operator-managed restart
- Inspect session state and compare pane-ID membership over time

Snapshot capture/inspection ships. Restore and restart execution do not: their
non-dry CLI paths fail closed before process or mux effects. The executable
restorer is library/test substrate, not a production recovery surface. This
system is designed for **mux topology + pane metadata**, not full process
checkpointing.

## What a snapshot contains

At a high level, a snapshot stores:

- **Layout topology**: deterministic window/tab grouping plus a size-inferred
  per-tab pane tree that can fall back to a flat layout (a `TopologySnapshot`)
- **Per-pane state schema**: pane id, cwd, optional command, terminal size +
  alt-screen flag, optional agent metadata, and optional redacted environment
  (a `PaneStateSnapshot`). Field presence depends on what the capture bridge
  actually supplied; a schema field is not proof that foreground process or
  agent continuity was captured.
- **Dedup/consistency witness**: a versioned, framed SHA-256 `state_hash` so
  identical snapshots can be skipped and persisted projections can be checked

The current topology schema v1 sorts numeric tab IDs for deterministic output.
It does not yet preserve user tab order or an incarnation-scoped active-tab
identity. The migration contract is
`docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md`.

What it does **not** (currently) guarantee:

- Restoring interactive in-process state (REPL variables, editor buffers, etc.)
- Restarting shells, agents, or other foreground processes automatically
- Restoring historical scrollback or terminal render state, regardless of capture quality
- Preserving mux-domain identity, durable app-reopen tab order, titles, exact
  cell geometry, stable active-tab identity, window/workspace placement, or
  full window appearance

## Quick start

### 1) Save a snapshot

```bash
ft snapshot save
```

JSON output:

```bash
ft snapshot save -f json
```

Example shape:

```json
{
  "ok": true,
  "session_id": "sess-…",
  "checkpoint_id": 123,
  "pane_count": 10,
  "pane_state_estimate_bytes": 123456,
  "persisted_text_bytes": 65432,
  "truncated_pane_count": 0,
  "projection_complete": true,
  "projection_completeness": "complete",
  "projection_completeness_scope": "persisted_pane_text_budget",
  "verification": "verified_v2",
  "trigger": "manual"
}
```

Triggers:

- `--trigger manual` (default)
- `--trigger event`
- `--trigger pre_restart`
- `--trigger pre_shutdown`
- `--trigger shutdown`
- `--trigger startup`

The one-shot CLI maps `event`, `pre_restart`, `pre_shutdown`, and `shutdown`
to ordinary `Manual` capture authority while retaining the requested label in
bounded checkpoint metadata. Those labels do not grant the watcher's sticky
terminal-capture reservation. `startup` maps to `SnapshotTrigger::Startup`;
the production periodic/intelligent scheduler also uses that trigger for its
first capture.

### 2) List snapshots

```bash
ft snapshot list --limit 10
```

JSON output:

```bash
ft snapshot list --limit 10 -f json
```

Example shape:

```json
{
  "ok": true,
  "count": 1,
  "limit": 10,
  "offset": 0,
  "has_more": false,
  "snapshots": [
    {
      "checkpoint_id": 123,
      "session_id": "sess-…",
      "checkpoint_at": 1730000000000,
      "checkpoint_type": "shutdown",
      "checkpoint_role": "snapshot",
      "pane_count": 10,
      "pane_state_estimate_bytes": 123456,
      "state_hash": "…",
      "label": "before maintenance",
      "verification": "not_computed",
      "projection_verification": "unchecked_projection",
      "projection_scope": "checkpoint_summary"
    }
  ],
  "verification": "not_computed",
  "projection_verification": "unchecked_projection",
  "projection_scope": "checkpoint_summary"
}
```

### 3) Inspect a snapshot

```bash
ft snapshot inspect 123
ft snapshot inspect 123 --pane 42
```

JSON output:

```bash
ft snapshot inspect 123 -f json
```

### 4) Diff two snapshots

```bash
ft snapshot diff 123 124
```

JSON output:

```bash
ft snapshot diff 123 124 -f json
```

This command compares pane-ID membership only. It does not compare topology,
working directories, commands, terminal state, agent metadata, or cell
content; structured output declares `comparison_scope: pane_membership_only`.

### 5) Delete a snapshot

```bash
ft snapshot delete 123
```

Use `--force` to skip confirmation:

```bash
ft snapshot delete 123 --force
```

Deletion is durable and can change exact clean-checkpoint authority and later
reconciliation decisions. Confirm the exact checkpoint identity and retain a
verified backup before bypassing the prompt.

## Restore behavior

### Unclean-session detection

`SessionRestorer` contains a fail-closed library path for detecting sessions
whose `shutdown_clean` flag is `0`, but `ft watch` does not currently call it
or offer an automatic restore prompt. Use `ft session doctor` and inspect the
candidate checkpoint. Recovery mutations remain manual; startup
notification/prompt integration and safe executable restore are tracked work.

### `ft snapshot restore`

`ft snapshot restore <id> --dry-run` resolves a bounded, metadata-only
checkpoint descriptor through a read-only connection and prints a descriptor
and status report. It does not decode topology, load the full checkpoint
projection, or constitute an execution preflight or success guarantee.

Every non-dry invocation fails closed on every platform before checkpoint
resolution, database mutation, subprocess launch, process discovery, or mux
operation. Robot checkpoint rollback has the same contract: metadata-only
dry-run planning, with non-dry execution unavailable.

The library/test layout substrate can exercise windows, tabs, splits, local
working directories, and an explicit per-tab active-pane identifier when one
is present. Current pane-list-derived schema v1 snapshots do not preserve user
tab order or stable active-tab identity, and they do not establish mux-domain
identity, window/workspace placement, titles, exact cell geometry, terminal
render state, historical scrollback, processes, or agents. Captured
`output_segments` are arbitrary stream fragments rather than an authoritative
terminal-state snapshot and are never sent through PTY input.

Use `--dry-run`; `--layout-only` is currently a reserved no-op and the output
is only the bounded descriptor/status report:

```bash
ft snapshot restore 123 --layout-only --dry-run
```

## `ft restart`

`ft restart` execution is currently unavailable and fails closed before lock
acquisition, process discovery, snapshot capture, signaling, or any mux
mutation. The existing implementation cannot authenticate one exact mux
endpoint, process incarnation, and relaunch plan, so an acknowledgement flag is
not sufficient to make execution safe.

`--dry-run` reports this unavailable status and the intended continuity gaps.
It performs no operation. A future restart design must bind an authenticated
endpoint to an exact PID/incarnation receipt and a verified relaunch plan before
any stop/start workflow can ship.

Examples:

```bash
ft restart --dry-run
```

## Configuration

Snapshots are configured in `ft.toml` under `[snapshots]`:

```toml
[snapshots]
enabled = true
interval_seconds = 300
max_concurrent_captures = 10
retention_count = 10
retention_days = 7
```

Notes:

- In the library/test substrate, layout reconstruction creates a pane's default
  shell. Production CLI execution is unavailable, and the process layer never
  types a captured shell or agent command into a PTY.
- Captured shells and agents always receive an explicit manual disposition so
  state that was not restored cannot be mistaken for success.
- The retired `[snapshots.process_relaunch]` table is rejected with a migration
  error. Delete it from existing configuration; no replacement launch setting
  exists because process and agent restoration is unavailable.
- The entire top-level `[session]` table is unsupported and rejected. Delete
  it, including any retired `session.restore_max_lines` setting. Historical
  scrollback replay has no supported output channel, so there is no active
  replay-size limit to configure.
- Retention is enforced by both `retention_count` and `retention_days`.

## Performance budgets and proof status

Criterion budgets for isolated snapshot components live in
`crates/frankenterm-core/benches/snapshot_engine.rs`:

- Topology capture: **p50 < 1ms**
- Pane state extraction: **p50 < 10µs per pane**
- Dedup hash: **p50 < 100µs**
- SQLite transaction: **p50 < 10ms**
- SQLite query + deserialize: **p50 < 5ms**

These values are design budgets, not proof that the current release meets them
on a particular machine or at large topology sizes. End-to-end snapshot save,
checkpoint load, and library-restorer exercises also include mux transport,
SQLite authority transactions, topology size, and filesystem effects; there is
currently no production restore latency to quote. Do not infer a production
latency or scale support claim until the corresponding retained target-class
benchmark/soak artifact is non-skipped and signed.

These component caps also do not qualify long-history operator paths.
`ft session list` and `ft session show` now return bounded pages (`--limit 50`
by default, at most 200 rows, with `--offset` capped at 100,000) from one
read snapshot per invocation. They no longer materialize an entire history,
but list ordering, exact counts, and per-row clean-authority verification still
perform work that scales with the stored population; offset pages can also
drift between invocations. `ft session doctor` still scans and revalidates the
full history. Keyset/snapshot-token pagination, maintained authority summaries,
and a bounded doctor remain open work; do not infer large-history
responsiveness from the isolated snapshot budgets or the row-output caps.
