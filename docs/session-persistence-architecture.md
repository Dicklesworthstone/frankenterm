# Session Persistence — Architecture

This document explains how ft captures session evidence and how its
non-production library/test restore substrate is structured. Snapshot
capture/inspection ships; production snapshot-restore, robot rollback, and
restart execution are unavailable and fail closed before process or mux
effects.

## Goals and non-goals

**Goals**

- Persist enough mux state to inspect layout + operator context after a crash
- Make restore decisions deterministic and auditable
- Avoid burdening the high-frequency ingest writer with snapshot I/O
- Keep a deterministic library/test substrate for developing a future safe
  executable restore contract

**Non-goals (for now)**

- Process checkpoint/restore (CRIU-style)
- Perfect fidelity for alt-screen / TUIs in scrollback
- “Continue the same agent session” semantics (agents require manual follow-up)

## Modules and responsibilities

### Capture

- `frankenterm_core::snapshot_engine::SnapshotEngine`
  - Orchestrates capture
  - Computes a versioned, framed SHA-256 `state_hash` for dedup and consistency
  - Writes snapshots to SQLite tables (`mux_sessions`, `session_checkpoints`, `mux_pane_state`)

- `frankenterm_core::session_topology::TopologySnapshot`
  - Serializes deterministic, pane-list-derived window/tab grouping and a
    size-inferred per-tab pane tree as JSON; inference can fall back to a flat
    tree when split structure cannot be established
  - Schema v1 numerically sorts tab IDs, so it does not preserve user tab order
    or stable active-tab identity. See
    `docs/proposals/ft-7xqz4-8-10-1-tab-order-authority-contract.md` for the
    authoritative successor contract.

- `frankenterm_core::session_pane_state::PaneStateSnapshot`
  - Defines per-pane fields for cwd, optional command, terminal state, optional
    agent metadata, and optional redacted env
  - Field presence depends on the capture bridge; the schema does not imply
    that a foreground command, agent session, or environment was populated

### Restore

- `frankenterm_core::session_restore`
  - Provides library support for detecting unclean sessions (`shutdown_clean = 0`)
  - Loads the latest checkpoint
  - Coordinates layout restoration, the fail-closed scrollback capability
    check, and process-disposition classification
  - Is not currently called by the production `ft watch` startup path
  - Its executable orchestration is not reachable from a production CLI path

- `frankenterm_core::restore_layout::LayoutRestorer`
  - In library/test exercises, recreates a windows/tabs/splits/local-CWD subset
    and honors an explicit per-tab active-pane identifier when one is present
    via mux operations (current backend: WezTerm/FrankenTerm)
  - Does not restore mux-domain identity, durable app-reopen tab order, titles,
    stable active-tab identity, workspace/window placement, exact cell
    geometry, terminal render state, or full window appearance
  - Produces an old-pane-id → new-pane-id mapping

- `frankenterm_core::restore_scrollback`
  - Fails closed without writing PTY input because no safe render-state restore
    channel currently exists

- `frankenterm_core::restore_process`
  - Classifies captured process state as skipped or requiring manual follow-up
  - Makes executable process plans unrepresentable; no PTY-input launch path exists

## Data flow

### Snapshot capture

```text
direct mux `ListPanesResponse` when available, otherwise an explicitly eligible
backend CLI-list fallback → Vec<PaneInfo>
  → TopologySnapshot::from_panes()
  → PaneStateSnapshot::from_pane_info() (per pane)
  → snapshot_dedup_witness(panes)
  → SQLite transaction:
       mux_sessions (upsert session row)
       session_checkpoints (insert checkpoint)
       mux_pane_state (insert per-pane rows)
  → retention pruning
```

### Production CLI dry-run flow

```text
ft session doctor / ft snapshot list
  → operator selects an exact checkpoint
  → ft snapshot restore <id> --layout-only --dry-run
  → read-only, bounded descriptor lookup
  → metadata-only descriptor/status report (no topology decode)
  → no full checkpoint decode, database mutation, subprocess, or mux operation
```

Every non-dry snapshot restore and robot rollback fails closed before even
resolving the requested checkpoint. `ft restart` execution is unavailable for
the same exact-endpoint/process-incarnation authority gap.

### Library/test execution substrate

```text
test/library caller selects an exact checkpoint
  → load and validate the full checkpoint projection
  → LayoutRestorer recreates topology
  → restore_scrollback capability check (mapped replay fails closed)
  → restore_process disposition classification
  → exact intent/outcome/lifecycle authority settles clean or reconciliation-required
```

`ft watch` startup notification/prompt wiring is pending. The presence of the
library detector is not evidence that automatic startup recovery ships.

## SQLite schema (conceptual)

The snapshot engine stores session data in three core tables:

- `mux_sessions`
  - `session_id` (primary key)
  - `created_at`, `last_checkpoint_at`
  - `shutdown_clean` (0 = recovery authority unresolved, 1 = the latest exact
    capture/restore receipt is resolved); after restore this is not proof that
    the original process session shut down cleanly or retained continuity
  - `topology_json` (serialized `TopologySnapshot`)
  - `clean_checkpoint_id` (exact snapshot/receipt authority when the clean
    state is currently resolved)
  - `ft_version`, `host_id` (for diagnostics / cross-host detection)

- `session_checkpoints`
  - `id` (primary key)
  - `session_id` (FK)
  - `checkpoint_at` (epoch ms)
  - `checkpoint_type` (`periodic|event|shutdown|startup`)
  - `state_hash` (versioned, framed SHA-256 witness)
  - `pane_count`, `total_bytes`
  - `checkpoint_role` (`snapshot|restore_intent|restore_receipt`)
  - `topology_json` (topology owned by this exact snapshot; absent on authority
    records)
  - `restore_intent_checkpoint_id` (exact causal intent settled by an outcome
    receipt)

- `mux_pane_state`
  - `checkpoint_id` (FK)
  - `pane_id`
  - `cwd`, `command`
  - `terminal_state_json`
  - `agent_metadata_json`
  - `env_json` (redacted)

Use `ft snapshot inspect <id> -f json` to obtain a consistency-verified,
bounded, redacted projection without direct SQL. The witness detects accidental
corruption; it is not writer authentication. Inspection intentionally does not
expose raw persisted strings byte-for-byte.

## Deduplication

`SnapshotEngine` computes a deterministic `state_hash` for the current pane set.
If the hash is unchanged from the last capture, the engine can skip writing a new checkpoint.

This prevents periodic snapshots from bloating the database when nothing has materially changed.

## Process restoration disposition

Process restoration in the library/test substrate classifies captured state;
it does not re-launch foreground processes:

- Layout reconstruction creates each pane's default shell at the validated cwd.
- Captured shells and agents always receive an explicit manual disposition so
  omitted process state cannot be mistaken for a successful restore.
- Pane snapshots may persist cwd, optional command, optional agent metadata,
  and optional redacted environment, and inspection surfaces may serialize
  redacted/sanitized forms of those fields. Launch plans, disposition reports,
  and diagnostics deliberately exclude captured commands, argv, cwd values,
  agent types, and free-form hints so they cannot become an executable or
  content-leaking process channel.
- Reports retain exact saturating totals plus at most 32 deterministic-prefix,
  content-free sample entries; consumers never infer totals from that sample.

The historical `[snapshots.process_relaunch]` table is removed and rejected
with a finite migration error. Delete it from existing configuration. A future
mux-native argv restoration design must introduce a new reviewed contract
rather than silently activating credential-bearing historical templates.
The entire top-level `[session]` table is unsupported and rejected, including
the historical `session.restore_max_lines` knob. Without a safe mux-owned
terminal-output restoration channel, accepting any session-restore
configuration would describe production behavior that does not exist.

## Bench budgets

Snapshot performance budgets are encoded as Criterion metadata in:

- `crates/frankenterm-core/benches/snapshot_engine.rs`

These are development budgets, not target-class production evidence or an
operator latency guarantee. Support claims require retained, non-skipped
benchmark/soak artifacts for the relevant topology and machine class.

They also do not prove long-history responsiveness. `ft session list` and
`ft session show` now materialize bounded pages (`--limit 50` by default, hard
maximum 200, and bounded `--offset`) under one read snapshot per invocation.
The list query's global ordering and exact count still scale with all sessions,
clean-authority verification scales with the returned page, checkpoint offsets
still require SQLite to skip rows, and separate page invocations have no shared
snapshot token. `ft session doctor` continues to scan and revalidate the full
history. Keyset/snapshot-token pagination, maintained authority summaries, and
a bounded doctor remain open work and are not qualified by the component
budgets.
