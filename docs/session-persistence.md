# Session Persistence (Snapshots)

ft’s session persistence system captures terminal backend mux state (current bridge: WezTerm) into SQLite snapshots so you can:

- Recover after an unclean shutdown (crash / power loss)
- Perform safer restarts by snapshotting first
- Inspect and diff session state over time

This system is designed for **mux topology + pane metadata**, not full process checkpointing.

## What a snapshot contains

At a high level, a snapshot stores:

- **Layout topology**: windows / tabs / split tree (a `TopologySnapshot`)
- **Per-pane state**: pane id, cwd, command, terminal size + alt-screen flag, agent metadata (a `PaneStateSnapshot`)
- **Dedup hash**: a BLAKE3 `state_hash` so identical snapshots can be skipped

What it does **not** (currently) guarantee:

- Restoring interactive in-process state (REPL variables, editor buffers, etc.)
- Restoring authenticated agent sessions (Claude/Codex/Gemini will start fresh)
- Perfect scrollback fidelity when capture data was already missing or truncated before the snapshot

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
- `--trigger startup` (used by the watcher on startup)

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

### Automatic restore on startup (watcher)

On startup, `ft watch` checks for sessions that did not shut down cleanly (`shutdown_clean = 0`).
If it finds one, it will **detect** that an unclean session exists and offer to restore from the latest checkpoint.

This remains the most automatic restore path.

### `ft snapshot restore`

`ft snapshot restore <id>` recreates the saved mux layout from a specific checkpoint.
By default it also replays captured scrollback for each pane up to the checkpoint’s recorded `scrollback_checkpoint_seq`.
This restore path is currently supported only on Unix platforms.

Use `--layout-only` when you want to restore the pane/window topology without replaying scrollback:

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
launch_shells = true
launch_agents = false
launch_delay_ms = 500
```

Notes:

- `launch_agents = false` by default because agent sessions don’t restore “where they left off”.
- Retention is enforced by both `retention_count` and `retention_days`.

## Performance expectations

Criterion budgets for core snapshot components live in `crates/frankenterm-core/benches/snapshot_engine.rs`:

- Topology capture: **p50 < 1ms**
- Pane state extraction: **p50 < 10µs per pane**
- Dedup hash: **p50 < 100µs**
- SQLite transaction: **p50 < 10ms**
- SQLite query + deserialize: **p50 < 5ms**

End-to-end `ft snapshot save` time is usually dominated by backend bridge CLI latency (currently WezTerm) and pane count.
As a rule of thumb, operators should expect a snapshot to complete in **well under a few seconds** for typical local sessions.
