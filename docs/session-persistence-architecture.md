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
- “Continue the same agent session” semantics (agents require manual follow-up)

## Modules and responsibilities

### Capture

- `frankenterm_core::snapshot_engine::SnapshotEngine`
  - Orchestrates capture
  - Computes a BLAKE3 `state_hash` for dedup (“skip if unchanged”)
  - Writes snapshots to SQLite tables (`mux_sessions`, `session_checkpoints`, `mux_pane_state`)

- `frankenterm_core::session_topology::TopologySnapshot`
  - Serializes mux layout (window/tab/split tree) as JSON
  - Schema v1 is pane-list-derived and numerically sorts tab IDs; it is
    deterministic but does not preserve user tab order or stable active-tab
    identity. See
    `docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md` for the
    authoritative successor contract.

- `frankenterm_core::session_pane_state::PaneStateSnapshot`
  - Captures per-pane metadata (cwd, command, terminal state, agent metadata, redacted env)

### Restore

- `frankenterm_core::session_restore`
  - Detects unclean sessions (`shutdown_clean = 0`)
  - Loads the latest checkpoint
  - Coordinates layout restoration, the fail-closed scrollback capability
    check, and process-disposition classification

- `frankenterm_core::restore_layout::LayoutRestorer`
  - Recreates windows/tabs/splits via backend bridge CLI operations (current: WezTerm)
  - Produces an old-pane-id → new-pane-id mapping

- `frankenterm_core::restore_scrollback`
  - Fails closed without writing PTY input because no safe render-state restore
    channel currently exists

- `frankenterm_core::restore_process`
  - Classifies captured process state as skipped or requiring manual follow-up
  - Rejects legacy executable plans without writing PTY input

## Data flow

### Snapshot capture

```text
backend bridge cli list (current: `wezterm cli list`) → Vec<PaneInfo>
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
  → restore_scrollback capability check (mapped replay fails closed)
  → restore_process disposition classification
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

## Process restoration disposition

Process restoration currently classifies captured state; it does not re-launch
foreground processes:

- Layout reconstruction creates each pane's default shell at the validated cwd.
- Captured shells and agents always receive an explicit manual disposition so
  omitted process state cannot be mistaken for a successful restore.
- Captured commands, cwd values, agent types, and manual hints are excluded from
  reports and diagnostics.
- Caller-supplied legacy executable plans fail closed and never write PTY input.

The historical `launch_shells`, `launch_agents`, `launch_delay_ms`, and
`agent_commands` configuration keys are reserved and ignored. Actual process
relaunch requires a future mux-native, argv-isolated spawn API. Configuration
lives under `[snapshots.process_relaunch]` in `ft.toml`.

## Bench budgets

Snapshot performance budgets are encoded as Criterion metadata in:

- `crates/frankenterm-core/benches/snapshot_engine.rs`

These are used as “operator expectations” and as a regression target during development.
