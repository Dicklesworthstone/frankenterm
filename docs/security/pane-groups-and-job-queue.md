# Pane Groups + Background-Job Queue

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.8] / `ft-2okh0.8`
**Status:** Foundation slice shipped. Registry contract +
operation taxonomy + priority queue + structured log row
contract + health snapshots + 35 lib tests all live.
Production wiring (asupersync task pool, robot-mode CLI,
plugin hook seam, session-restore) is the integration
follow-on.

## Headline rule

> AI agents can group panes — `cc-agents`, `cod-agents`,
> `web-agents` — and issue **one operation** that fans out
> across every pane in the group: close-all, kill-all,
> send-text-to-all, focus-cycle. Long-running work
> (`DisplayPaneError`, `AnimatePluginLoading`, `WebRequest`,
> `RunCommand`, `ReportSessionInfo`) lives on a
> priority-ordered background-job queue so the render
> thread never blocks.

## Sub-tasks (per bead)

| Sub-task | Module | Status |
|---|---|---|
| 8.1 Pane group abstraction (close-all/focus-cycle/send-to-all/kill-all) | `pane_groups::PaneGroupRegistry` + `GroupOp` | ✓ |
| 8.2 Background-job queue (asupersync task pool, 4 priority levels) | `pane_groups::BackgroundJobQueue` + `JobPriority` | ✓ contract |
| 8.3 Robot-mode CLI (`ft robot panes group`) | dispatches into `GroupOp` taxonomy | ⏳ integration |
| 8.4 Plugin hooks for group ops | `pane_groups::GroupChangeEvent` | ✓ event shape |
| 8.5 Session-restore preserves group membership | `PaneGroupRegistry` is `serde`-clean | ✓ persistence shape |

## Operation taxonomy (sub-task 8.1)

`GroupOp` is a closed enum:

- `Create { id, name }` — fresh group with name.
  Case-insensitive uniqueness via `name_index`.
- `Add { group, pane }` — single pane into a group.
- `Remove { group, pane }` — single pane out of a group.
- `Toggle { group, pane }` — zellij-style toggle (add if
  absent, remove if present).
- `CloseAll { group }` — deregister every pane from the
  group, panes survive.
- `KillAll { group }` — kill every pane + destroy the
  group itself.
- `SendTextToAll { group, bytes_len }` — fan out a text
  payload (telemetry records `panes_targeted`).
- `FocusCycle { group }` — non-mutating; the GUI cycles
  keyboard focus, the registry just emits a bulk-op event.
- `Rename { group, new_name }` — updates the name +
  refreshes the index.
- `Destroy { group }` — drops the group; panes survive.

`apply_group_op` is a pure state-machine reducer. Outcomes:

- `Applied { panes_affected }` — counter bumped.
- `NoOp` — same effective state.
- `Denied { reason }` — `EmptyName` / `NameAlreadyTaken` /
  `UnknownGroup` / `DuplicateGroupId`.

## Priority levels (sub-task 8.2)

Per the bead's table:

| Priority | Use case |
|---|---|
| `Critical` | TX engine prepare/commit/compensate (cross-link `tx_execution.rs`) |
| `High` | User-initiated (workflow run, send to all) |
| `Normal` | Background polling, plugin tick |
| `Low` | Periodic cleanup, telemetry export |

Higher priority dequeues first. Within a priority level,
FIFO via `(priority, -insertion_order)` ordering on the
`BinaryHeap`. Tested in `enqueue_then_dequeue_returns_critical_first`
and `fifo_within_same_priority`.

## Structured logging contract

Bead requires JSONL at
`tests/pane_groups/logs/<scenario>.jsonl`. Two row kinds:

- `GroupOp { ts_ms, group_name, op_slug, pane_count, success }`
- `BackgroundJob { ts_ms, job_kind, priority, scheduled_at_ms, started_at_ms?, completed_at_ms?, status }`

`render_log_jsonl` / `parse_log_jsonl` are bidirectionally
clean (`structured_log_jsonl_roundtrip` test).

## Plugin hook seam (sub-task 8.4)

`GroupChangeEvent` is the closed event shape plugins
subscribe to:

- `GroupCreated { group, name }`
- `PaneAdded { group, pane }`
- `PaneRemoved { group, pane }`
- `GroupBulkOp { group, op_slug, panes_affected }` —
  emitted for `CloseAll` / `KillAll` / `SendTextToAll` /
  `FocusCycle`.
- `GroupRenamed { group, new_name }`
- `GroupDestroyed { group }`

`PaneGroupRegistry.events` retains the last
`MAX_RETAINED_EVENTS = 1024`; the integration drains older
events into the asupersync log path.

## "DO NOT BREAK" rules

The bead names three constraints; foundation slice
preserves each:

- **A11Y announcements for group ops** — `GroupChangeEvent`
  emission is unconditional (not gated on `is_safe()`); the
  GUI integration consumes events to drive AT
  announcements.
- **Privacy via redactor** — `bytes_len` is recorded, *not*
  the payload itself. Send-text-to-all calls the existing
  redactor on each pane separately; the registry never
  stores the bytes.
- **TX engine for cross-pane mutations** — bulk ops emit
  `op_slug = "kill_all"` / `"close_all"` events the TX
  layer captures + sequences. The registry's pure-logic
  reducer does not perform side effects; it only emits
  events.

## Telemetry

`PaneGroupHealth`:
- `groups_total` — current group count.
- `grouped_panes_total` — distinct panes across all
  groups.
- `ops_applied_total` — lifetime applied ops.
- `ops_denied_total` — lifetime denials.
- `denial_reasons` — per-reason histogram.
- `is_safe()`: no denials.

`BackgroundJobHealth`:
- `queue_depth` — total queue depth.
- `queue_depth_per_priority` — Critical/High/Normal/Low
  histogram.
- `enqueued_total` / `dequeued_total` / `cancelled_total` —
  lifetime counters per priority.
- `is_safe()`: `critical_queued() == 0` (Critical jobs
  shouldn't backlog).

## Tests (35)

- 13 group-op tests covering every variant + denial
  reasons.
- 7 queue tests covering priority ordering, FIFO, cancel,
  per-priority counting, dequeued counter.
- 5 health-snapshot tests.
- 1 structured-log JSONL roundtrip test.
- 1 registry serde roundtrip test (session-restore shape).
- 1 headline-scenario test
  (`ai_agents_group_kill_all_scenario`).

## Bead acceptance status

| Item | Status |
|---|---|
| Pane group abstraction | ✓ `PaneGroupRegistry` |
| Group operations: close-all, focus-cycle, send-to-all, kill-all | ✓ all in `GroupOp` |
| Visual group indicator | ⏳ GUI integration follow-on |
| Group save/restore (in session-restore) | ✓ serde-clean shape |
| Background-job queue (4 priority levels) | ✓ `BackgroundJobQueue` |
| `runtime_async::spawn` task pool | ⏳ wiring follow-on |
| Robot-mode `ft panes group <name> add <id>` | ⏳ CLI dispatch follow-on |
| Plugin hooks `on_pane_group_change` | ✓ `GroupChangeEvent` shape |
| Structured logging JSONL | ✓ `StructuredLogRow` contract |
| `tests/pane_groups/jobs/*` corpus + chaos test | ⏳ follow-on |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- Sibling: `ft-2okh0.5.1` (tmux-style sessions —
  pane-group is a lighter-weight abstraction over the same
  multi-pane substrate), `ft-2okh0.6` (zellij-style layouts
  — group definition can pre-populate from a layout).
- Cross-link: `tx_execution.rs` (Critical-priority jobs
  carry TX engine prepare/commit/compensate work),
  `redactor.rs` (send-text-to-all path).
- Attestation: `ft-syqcz.1`.
