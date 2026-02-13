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
- A working `ft snapshot restore` CLI path (see “Restore behavior” below)

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

This is currently the supported restore path.

### `ft snapshot restore` (not wired yet)

`ft snapshot restore <id>` currently exits with an error and points you to the watcher’s restore-on-startup flow.

## “Safe restart” workflow (current)

`ft restart` exists, but is not fully wired yet. The current safe workflow is:

1) Capture a pre-restart snapshot:
   ```bash
   ft snapshot save --trigger pre_restart
   ```
2) Stop the watcher (optional, but reduces DB contention):
   ```bash
   ft stop
   ```
3) Restart WezTerm / mux server using your normal process
4) Start the watcher:
   ```bash
   ft watch
   ```
5) If an unclean shutdown is detected, follow the restore prompt

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
