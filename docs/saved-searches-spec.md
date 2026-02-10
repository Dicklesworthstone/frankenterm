# Saved Searches + Scheduling Semantics

**Bead:** bd-yt9x
**Author:** MagentaWaterfall
**Date:** 2026-02-01
**Status:** Draft

## Overview

This document defines the **saved search** data model and **scheduling semantics** for `wa`.
The goal is to make search reuse and scheduled alerts deterministic across CLI/TUI/web, while
reusing the existing FTS search pipeline and redaction rules.

## Design Principles

1. **Read-only by default**: Saved searches never mutate state.
2. **Deterministic**: Stable schema, canonical field ordering, and repeatable results.
3. **Single search engine**: Saved searches must call the same core search APIs as `ft search`.
4. **Redaction-first**: Any preview or alert payload must be redacted before persistence.
5. **Scheduling is simple**: Fixed-interval scheduling only (no cron in v1).

## Schema

### Table: `saved_searches`

| Field | Type | Notes |
| --- | --- | --- |
| `id` | TEXT (PK) | UUID or stable hash; opaque identifier |
| `name` | TEXT (unique) | Human-friendly name |
| `query` | TEXT | FTS query string (same as `ft search`) |
| `pane_id` | INTEGER NULL | Optional scope to a pane |
| `limit` | INTEGER | Max rows returned (default 50) |
| `since_mode` | TEXT | `last_run` (default) or `fixed` |
| `since_ms` | INTEGER NULL | Only used when `since_mode=fixed` |
| `schedule_interval_ms` | INTEGER NULL | Null = manual only |
| `enabled` | INTEGER | 1/0; scheduler only considers enabled rows |
| `last_run_at` | INTEGER NULL | Epoch ms of last run start |
| `last_result_count` | INTEGER NULL | Cached result count for UX |
| `last_error` | TEXT NULL | Cached error summary |
| `created_at` | INTEGER | Epoch ms |
| `updated_at` | INTEGER | Epoch ms |

Notes:
- `since_mode=last_run` uses `last_run_at` as the lower bound.
- `schedule_interval_ms` must be >= minimum interval (see Scheduling).
- `query` is the canonical FTS expression; filters like pane scope are separate.

### Table: `saved_search_runs`

| Field | Type | Notes |
| --- | --- | --- |
| `id` | INTEGER (PK) | Auto-increment |
| `search_id` | TEXT | FK to `saved_searches.id` |
| `started_at` | INTEGER | Epoch ms |
| `finished_at` | INTEGER | Epoch ms |
| `status` | TEXT | `ok`, `no_results`, `error` |
| `result_count` | INTEGER | Total matches |
| `preview_json` | TEXT NULL | Redacted preview payload |
| `error_code` | TEXT NULL | Stable error code if failed |
| `error_message` | TEXT NULL | Short error summary |

Notes:
- `preview_json` must be redacted and truncated.
- `saved_search_runs` is optional but recommended for auditability and UI history.

## Scheduling Semantics

### Fixed-Interval Execution

- A saved search is **scheduled** if `schedule_interval_ms` is set and `enabled=1`.
- A saved search is **due** if:
  - `last_run_at` is NULL, **or**
  - `now_ms - last_run_at >= schedule_interval_ms`

### Minimum Interval

To prevent runaway schedules, enforce a minimum interval (suggested: **30 seconds**). If a user
sets a smaller interval, clamp to the minimum and record a warning in `last_error`.

### Since Window Resolution

- If `since_mode=last_run`:
  - `since_ms = last_run_at` (or `now_ms - schedule_interval_ms` if `last_run_at` is NULL)
  - Apply a small overlap window (e.g., 1000ms) to avoid missing boundary events.
- If `since_mode=fixed`:
  - Use the stored `since_ms` as-is.

### Execution Order and Concurrency

- Scheduler should execute searches **sequentially** within a single process to preserve ordering
  and reduce load.
- If concurrent execution is introduced later, guard with a per-search lock and ensure
  `last_run_at` is updated atomically.

## Alert Payload Shape (Redacted)

All scheduled alerts should reuse the search pipeline and produce a **redacted** payload with
stable ordering. Suggested JSON shape:

```json
{
  "search_id": "ss_123",
  "name": "codex-errors",
  "query": "error OR failure",
  "pane_id": 0,
  "window": { "since_ms": 1700000000000, "until_ms": 1700000300000 },
  "result_count": 12,
  "preview": [
    {
      "pane_id": 0,
      "captured_at": 1700000000123,
      "snippet": "...redacted line snippet..."
    }
  ]
}
```

Rules:
- `preview` should be truncated to a small number of hits (default 5).
- `snippet` must pass through the same redactor as `ft search` output.
- All fields are optional-friendly for forward compatibility.

## CLI/TUI/Web Implications (Non-binding)

- `ft search save <name> <query>` should create a row in `saved_searches`.
- `ft search run <name>` should execute once and update `last_run_at`/`last_result_count`.
- `ft search schedule <name> --interval 60s` should set `schedule_interval_ms` and enable.
- UI surfaces should show `last_run_at`, `last_result_count`, and `last_error`.

## Edge Cases

- **No results**: store `status=no_results` and `result_count=0`.
- **Invalid query**: store `status=error` with stable error code and remediation hint.
- **Pane missing**: treat as error; keep schedule enabled but record `last_error`.
- **Redaction failure**: hard fail the run; never persist unredacted preview.

## Acceptance Criteria

- Schema is documented and stable.
- Scheduling rules are explicit and deterministic.
- Alert payload includes redacted previews and is safe for downstream channels.
- Implementation can reuse existing `ft search` core logic without duplication.
