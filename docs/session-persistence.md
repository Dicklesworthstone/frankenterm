# Session Persistence (Snapshots)

ft’s partially supported session persistence system captures terminal backend
mux state (current bridge: WezTerm) into SQLite snapshots so you can:

- Manually reconstruct the currently supported layout subset after an unclean shutdown
- Preserve evidence before an operator-managed restart
- Inspect and diff session state over time

This system is designed for **mux topology + pane metadata**, not full process checkpointing.

## What a snapshot contains

At a high level, a snapshot stores:

- **Layout topology**: windows / tabs / split tree (a `TopologySnapshot`)
- **Per-pane state**: pane id, cwd, command, terminal size + alt-screen flag, agent metadata (a `PaneStateSnapshot`)
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
  cell geometry, or full window appearance

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
  "total_bytes": 123456,
  "trigger": "Manual"
}
```

Triggers:

- `--trigger manual` (default)
- `--trigger pre_restart` (recommended before a manual restart)
- `--trigger startup` (reserved trigger label; no production watcher caller currently captures it)

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
  "count": 2,
  "snapshots": [
    {
      "checkpoint_id": 123,
      "session_id": "sess-…",
      "checkpoint_at": 1730000000000,
      "checkpoint_type": "shutdown",
      "pane_count": 10,
      "total_bytes": 123456,
      "state_hash": "…"
    }
  ]
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

### 5) Delete a snapshot

```bash
ft snapshot delete 123
```

Use `--force` to skip confirmation:

```bash
ft snapshot delete 123 --force
```

## Restore behavior

### Unclean-session detection

`SessionRestorer` contains a fail-closed library path for detecting sessions
whose `shutdown_clean` flag is `0`, but `ft watch` does not currently call it
or offer an automatic restore prompt. Use `ft session doctor`, inspect the
candidate checkpoint, and invoke `ft snapshot restore <id> --layout-only`
manually. Startup notification/prompt integration remains tracked work.

### `ft snapshot restore`

`ft snapshot restore <id>` attempts the currently supported subset of the saved
mux layout from a specific checkpoint: windows, tabs, pane splits, local
working directories, and active-pane/tab selection. It does not yet restore mux
domain identity, titles, window appearance, exact cell geometry, terminal
render state, historical scrollback, or processes.

The CLI currently forces layout-only operation on Unix. Captured
`output_segments` remain persisted data, but they are arbitrary stream
fragments rather than an authoritative terminal-state snapshot and are never
sent through PTY input. The direct library path fails closed if scrollback
restoration is requested. Non-Unix restore is rejected.

Use `--layout-only` to state the current operator mode explicitly:

```bash
ft snapshot restore 123 --layout-only
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

- Layout restoration creates the pane's default shell. The process layer never
  types a captured shell or agent command into that PTY.
- Captured shells and agents always receive an explicit manual disposition so
  state that was not restored cannot be mistaken for success.
- The retired `[snapshots.process_relaunch]` table is rejected with a migration
  error. Delete it from existing configuration; no replacement launch setting
  exists because process and agent restoration is unavailable.
- The retired `session.restore_max_lines` setting is likewise rejected instead
  of being silently ignored. Historical scrollback replay has no supported
  output channel, so there is no active replay-size limit to configure.
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
on a particular machine or at large topology sizes. End-to-end save/restore
also includes mux transport, SQLite authority transactions, topology size, and
filesystem effects. Do not infer a production latency or scale support claim
until the corresponding retained target-class benchmark/soak artifact is
non-skipped and signed.
