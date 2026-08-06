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
- **Dedup hash**: a BLAKE3 `state_hash` so identical snapshots can be skipped

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

`ft restart` performs the built-in safe restart flow:

1) Capture a pre-restart snapshot
2) Stop `frankenterm-mux-server`
3) Start `frankenterm-mux-server`
4) Restore from the captured snapshot unless `--skip-restore` is set

Like `ft snapshot restore`, `ft restart` is currently supported only on Unix platforms.

Examples:

```bash
ft restart
ft restart --layout-only
ft restart --skip-restore
```

If the mux restart succeeds but the restore phase fails, the snapshot is preserved and the CLI prints the checkpoint ID for manual recovery with `ft snapshot restore <id>`.

## Configuration

Snapshots are configured in `ft.toml` under `[snapshots]`:

```toml
[snapshots]
enabled = true
interval_seconds = 300
max_concurrent_captures = 10
retention_count = 10
retention_days = 7

[snapshots.process_relaunch]
# Reserved historical keys; neither one permits execution.
launch_shells = true
launch_agents = false
```

Notes:

- Layout restoration creates the pane's default shell. The process layer never
  types a captured shell or agent command into that PTY.
- Captured shells and agents always receive an explicit manual disposition so
  state that was not restored cannot be mistaken for success.
- All four historical keys (`launch_shells`, `launch_agents`, `launch_delay_ms`,
  and `agent_commands`) are reserved and ignored until FrankenTerm has a
  mux-native, argv-isolated spawn API.
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
