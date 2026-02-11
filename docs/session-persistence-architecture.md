# Session Persistence — Architecture

This document explains how ft captures and restores session state for crash recovery and safe restarts.

## Goals and non-goals

**Goals**

- Persist enough mux state to reconstruct layout + operator context after a crash
- Make restore decisions deterministic and auditable
- Avoid burdening the high-frequency ingest writer with snapshot I/O

**Non-goals (for now)**

- Process checkpoint/restore (CRIU-style)
- Perfect fidelity for alt-screen / TUIs in scrollback
- “Continue the same agent session” semantics (agents relaunch fresh)

## Modules and responsibilities

### Capture

- `frankenterm_core::snapshot_engine::SnapshotEngine`
  - Orchestrates capture
  - Computes a BLAKE3 `state_hash` for dedup (“skip if unchanged”)
  - Writes snapshots to SQLite tables (`mux_sessions`, `session_checkpoints`, `mux_pane_state`)

- `frankenterm_core::session_topology::TopologySnapshot`
  - Serializes mux layout (window/tab/split tree) as JSON

- `frankenterm_core::session_pane_state::PaneStateSnapshot`
  - Captures per-pane metadata (cwd, command, terminal state, agent metadata, redacted env)

### Restore

- `frankenterm_core::session_restore`
  - Detects unclean sessions (`shutdown_clean = 0`)
  - Loads the latest checkpoint
  - Coordinates restoration steps (layout first; optional scrollback/process steps)

- `frankenterm_core::restore_layout::LayoutRestorer`
  - Recreates windows/tabs/splits via WezTerm CLI operations
  - Produces an old-pane-id → new-pane-id mapping

- `frankenterm_core::restore_scrollback`
  - Replays captured scrollback into newly created panes (best-effort)

- `frankenterm_core::restore_process`
  - Plans optional process re-launch (shells by default; agents opt-in)

## Data flow

### Snapshot capture

```text
wezterm cli list → Vec<PaneInfo>
  → TopologySnapshot::from_panes()
  → PaneStateSnapshot::from_pane_info() (per pane)
  → compute_state_hash(panes)
  → SQLite transaction:
       mux_sessions (upsert session row)
       session_checkpoints (insert checkpoint)
       mux_pane_state (insert per-pane rows)
  → retention pruning
```

### Restore on startup

```text
ft watch startup
  → find sessions where shutdown_clean = 0
  → load_latest_checkpoint(session_id)
  → LayoutRestorer recreates topology
  → (optional) restore_scrollback
  → (optional) restore_process relaunch
  → mark session shutdown_clean = 1
```

## SQLite schema (conceptual)

The snapshot engine stores session data in three core tables:

- `mux_sessions`
  - `session_id` (primary key)
  - `created_at`, `last_checkpoint_at`
  - `shutdown_clean` (0 = crash/unclean, 1 = clean)
  - `topology_json` (serialized `TopologySnapshot`)
  - `ft_version`, `host_id` (for diagnostics / cross-host detection)

- `session_checkpoints`
  - `id` (primary key)
  - `session_id` (FK)
  - `checkpoint_at` (epoch ms)
  - `checkpoint_type` (`periodic|event|shutdown|startup`)
  - `state_hash` (BLAKE3)
  - `pane_count`, `total_bytes`

- `mux_pane_state`
  - `checkpoint_id` (FK)
  - `pane_id`
  - `cwd`, `command`
  - `terminal_state_json`
  - `agent_metadata_json`
  - `env_json` (redacted)

Use `ft snapshot inspect <id> -f json` to see the persisted values without direct SQL.

## Deduplication

`SnapshotEngine` computes a deterministic `state_hash` for the current pane set.
If the hash is unchanged from the last capture, the engine can skip writing a new checkpoint.

This prevents periodic snapshots from bloating the database when nothing has materially changed.

## Process re-launch (opt-in)

Process relaunch is intentionally conservative:

- Shell relaunch is allowed by default (best-effort, cwd-based)
- Agent relaunch is **disabled by default** because the new process cannot recover hidden model state

Configuration lives under `[snapshots.process_relaunch]` in `ft.toml`.

## Bench budgets

Snapshot performance budgets are encoded as Criterion metadata in:

- `crates/frankenterm-core/benches/snapshot_engine.rs`

These are used as “operator expectations” and as a regression target during development.

