# Query Facade Contract: Stable TUI Data Boundary

**Bead:** wa-5htt (FTUI-04.1)
**Date:** 2026-02-09

## Purpose

This document formalizes the `QueryClient` trait as the stable data boundary
between TUI rendering code and wa-core's storage/IPC layers. The ftui migration
replaces everything above this boundary (rendering, terminal management, event
loop) while leaving the boundary itself and everything below it unchanged.

The contract defined here is the acceptance standard for FTUI-05 (view migration):
every ftui view must consume data exclusively through `QueryClient` methods.

## Trait Definition

```rust
pub trait QueryClient: Send + Sync {
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError>;
    fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError>;
    fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError>;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError>;
    fn health(&self) -> Result<HealthStatus, QueryError>;
    fn is_watcher_running(&self) -> bool;
    fn mark_event_muted(&self, event_id: i64) -> Result<(), QueryError>;
    fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError>;
    fn list_action_history(&self, limit: usize) -> Result<Vec<HistoryEntryView>, QueryError>;
    fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkView>, QueryError>;
    fn list_saved_searches(&self) -> Result<Vec<SavedSearchView>, QueryError>;
    fn ruleset_profile_state(&self) -> Result<RulesetProfileState, QueryError>;
}
```

**Location:** `crates/wa-core/src/tui/query.rs`

**Bounds:** `Send + Sync` required because the TUI event loop may call methods
from a thread other than main (e.g., data refresh timer).

## Method Contract

### Read Methods (Pure Queries)

| Method | Returns | View Consumer | Filters | Default Impl |
|--------|---------|--------------|---------|-------------|
| `list_panes` | `Vec<PaneView>` | Home, Panes | None (returns all) | Required |
| `list_events` | `Vec<EventView>` | Events | `EventFilters` (pane, rule, type, unhandled, limit) | Required |
| `list_triage_items` | `Vec<TriageItemView>` | Triage | None (pre-filtered) | Required |
| `search` | `Vec<SearchResultView>` | Search | `query: &str`, `limit: usize` | Required |
| `health` | `HealthStatus` | Home | None | Required |
| `is_watcher_running` | `bool` | Home | None | Required |
| `list_active_workflows` | `Vec<WorkflowProgressView>` | Triage, Home | None | Required |
| `list_action_history` | `Vec<HistoryEntryView>` | History | `limit: usize` | `Ok(Vec::new())` |
| `list_pane_bookmarks` | `Vec<PaneBookmarkView>` | Panes | None | `Ok(Vec::new())` |
| `list_saved_searches` | `Vec<SavedSearchView>` | Search | None | `Ok(Vec::new())` |
| `ruleset_profile_state` | `RulesetProfileState` | Home | None | `Ok(default)` |

### Write Methods (Side Effects)

| Method | Effect | View Consumer |
|--------|--------|--------------|
| `mark_event_muted` | Marks event handled + adds mute record | Triage (`m` key) |

### Default Implementations

Four methods provide default implementations returning empty/default values:

- `list_action_history` -> `Ok(Vec::new())`
- `list_pane_bookmarks` -> `Ok(Vec::new())`
- `list_saved_searches` -> `Ok(Vec::new())`
- `ruleset_profile_state` -> `Ok(RulesetProfileState::default())`

These defaults exist because these features were added after the initial trait.
Mock implementations need not override them unless testing those features.

## Error Model

```rust
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("Watcher is not running")]
    WatcherNotRunning,

    #[error("Database not initialized: {0}")]
    DatabaseNotInitialized(String),

    #[error("WezTerm error: {0}")]
    WeztermError(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Query failed: {0}")]
    QueryFailed(String),
}
```

### Error Handling Rules

1. **Views must handle all variants gracefully.** No `QueryError` should panic
   or crash the TUI. Display an inline error message and allow retry via `r`.

2. **`WatcherNotRunning`** indicates the watcher daemon is not active. Views
   should show a "start watcher" prompt, not an error state.

3. **`DatabaseNotInitialized`** means no SQLite database is available. Views
   that need storage (Events, Triage, Search, History) should show an empty
   state with guidance. Panes (from WezTerm IPC) may still work.

4. **`WeztermError`** means WezTerm CLI communication failed. Home and Panes
   views show degraded state. Other views are unaffected.

5. **`StorageError`** is a catch-all for SQLite failures. Treat as transient;
   retry on next refresh cycle.

6. **`QueryFailed`** is a catch-all for other failures (e.g., config resolution).
   Log and display.

## Response Types

### PaneView

| Field | Type | Source | Notes |
|-------|------|--------|-------|
| `pane_id` | `u64` | WezTerm IPC | Stable pane identifier |
| `title` | `String` | WezTerm IPC | Current pane title (may change) |
| `domain` | `String` | WezTerm IPC | `"local"` or `"ssh:<host>"` |
| `cwd` | `Option<String>` | WezTerm IPC | Working directory if available |
| `is_excluded` | `bool` | Computed | Always `false` currently |
| `agent_type` | `Option<String>` | Inferred | `"codex"`, `"claude"`, `"gemini"`, or `None` |
| `pane_state` | `String` | Inferred | `"AltScreen"`, `"CommandRunning"`, `"PromptActive"`, `"unknown"` |
| `last_activity_ts` | `Option<i64>` | Storage | Epoch ms of last capture, `None` without DB |
| `unhandled_event_count` | `u32` | Storage | Count of unhandled events, `0` without DB |

Agent type inference: scans `title` and `cwd` (case-insensitive) for `"codex"`,
`"claude"`, `"gemini"`. First match wins. Returns `None` for unknown agents.

Pane state inference priority: alt-screen active -> `"AltScreen"`, cursor hidden
-> `"CommandRunning"`, is_active -> `"PromptActive"`, else `"unknown"`.

### EventView

| Field | Type | Source |
|-------|------|--------|
| `id` | `i64` | Storage (auto-increment) |
| `rule_id` | `String` | Pattern engine rule identifier |
| `pane_id` | `u64` | Pane where detection occurred |
| `severity` | `String` | `"error"`, `"warning"`, `"info"` |
| `message` | `String` | Matched text or `"Pattern matched"` fallback |
| `timestamp` | `i64` | Detection epoch ms |
| `handled` | `bool` | `true` if `handled_at` is set |
| `triage_state` | `Option<String>` | From event annotations |
| `labels` | `Vec<String>` | From event annotations |
| `note` | `Option<String>` | From event annotations |

### EventFilters

| Field | Type | Default | Effect |
|-------|------|---------|--------|
| `pane_id` | `Option<u64>` | `None` | Filter to specific pane |
| `rule_id` | `Option<String>` | `None` | Filter to specific rule |
| `event_type` | `Option<String>` | `None` | Filter by event type |
| `unhandled_only` | `bool` | `false` | Only unhandled events |
| `limit` | `usize` | `0` | Max results (0 = no limit) |

### TriageItemView

| Field | Type | Notes |
|-------|------|-------|
| `section` | `String` | `"health"`, `"crashes"`, `"events"`, `"workflows"` |
| `severity` | `String` | `"error"`, `"warning"`, `"info"` |
| `title` | `String` | Human-readable summary |
| `detail` | `String` | Extended description (truncated to 120 chars for events) |
| `actions` | `Vec<TriageAction>` | Suggested wa CLI commands |
| `event_id` | `Option<i64>` | Source event if applicable |
| `pane_id` | `Option<u64>` | Source pane if applicable |
| `workflow_id` | `Option<String>` | Source workflow if applicable |

`TriageAction`: `{ label: String, command: String }` - label for display,
command for execution via TUI command handoff.

Triage items are sorted by severity (error > warning > info), then title.

### SearchResultView

| Field | Type | Notes |
|-------|------|-------|
| `pane_id` | `u64` | Pane containing the match |
| `timestamp` | `i64` | Capture epoch ms |
| `snippet` | `String` | FTS5 snippet with `>>..<<` highlight markers |
| `rank` | `f64` | BM25 relevance score |

Search uses FTS5 with `snippet_max_tokens=30`, `highlight_prefix=">>"`,
`highlight_suffix="<<"`.

### WorkflowProgressView

| Field | Type | Notes |
|-------|------|-------|
| `id` | `String` | Workflow instance identifier |
| `workflow_name` | `String` | Workflow definition name |
| `pane_id` | `u64` | Target pane |
| `current_step` | `usize` | 0-based step index |
| `total_steps` | `usize` | Estimated total (may be `current_step + 1`) |
| `status` | `String` | Workflow status string |
| `error` | `Option<String>` | Error message if failed |
| `started_at` | `i64` | Epoch ms |
| `updated_at` | `i64` | Epoch ms |

### HistoryEntryView

| Field | Type | Notes |
|-------|------|-------|
| `audit_id` | `i64` | Audit action record ID |
| `timestamp` | `i64` | Epoch ms |
| `pane_id` | `Option<u64>` | Associated pane |
| `workflow_id` | `Option<String>` | Associated workflow |
| `action_kind` | `String` | `"send_text"`, `"workflow_step"`, etc. |
| `result` | `String` | `"success"`, `"denied"`, `"failed"` |
| `actor_kind` | `String` | `"human"`, `"robot"`, `"mcp"`, `"workflow"` |
| `step_name` | `Option<String>` | Workflow step name |
| `undoable` | `bool` | `true` if undo is available and not yet executed |
| `undone` | `bool` | `true` if undo has been executed |
| `undo_strategy` | `Option<String>` | `"manual"`, `"workflow_abort"`, etc. |
| `undo_hint` | `Option<String>` | Redacted undo guidance |
| `rule_id` | `Option<String>` | Policy rule that triggered this action |
| `summary` | `String` | Best-effort: `input_summary || verification_summary || decision_reason` |

### HealthStatus

| Field | Type | Notes |
|-------|------|-------|
| `watcher_running` | `bool` | Lock file exists |
| `db_accessible` | `bool` | Database file exists |
| `wezterm_accessible` | `bool` | `list_panes()` succeeds with results |
| `wezterm_circuit` | `CircuitBreakerStatus` | Circuit breaker state for WezTerm IPC |
| `pane_count` | `usize` | Number of WezTerm panes |
| `event_count` | `usize` | Currently always `0` (placeholder) |
| `last_capture_ts` | `Option<i64>` | Currently always `None` (placeholder) |

### Shared Types (from ui_query.rs)

These types live in `crates/wa-core/src/ui_query.rs` and are framework-agnostic:

**PaneBookmarkView:** `{ pane_id, alias, tags, description, created_at, updated_at }`

**SavedSearchView:** `{ id, name, query, pane_id, limit, since_mode, since_ms,
schedule_interval_ms, enabled, last_run_at, last_result_count, last_error,
created_at, updated_at }`

**RulesetProfileState:** `{ active_profile, active_last_applied_at, profiles: Vec<RulesetProfileSummary> }`
- Default: `active_profile = "default"`, single implicit default profile
- Active profile selection: greatest `last_applied_at` timestamp, ties broken
  lexicographically by name

## Production Implementation

`ProductionQueryClient` bridges sync TUI code with async wa-core operations.

### Architecture

```
TUI Thread                    ProductionQueryClient               wa-core (async)
    |                                 |                                 |
    |-- list_panes() --------------->|                                 |
    |                                |-- runtime.block_on(async {     |
    |                                |     wezterm.list_panes()  ---->|
    |                                |     storage.count_unhandled -->|
    |                                |     storage.get_last_activity->|
    |                                |   }) <--------------------------|
    |<-- Vec<PaneView> --------------|                                 |
```

### Key Design Decisions

1. **Dedicated tokio runtime.** `ProductionQueryClient` owns a separate
   `tokio::runtime::Runtime` (2 worker threads, named `tui-query-runtime`).
   This avoids "cannot start a runtime from within a runtime" panics when
   the TUI runs in a separate thread from the main async context.

2. **Sync trait surface.** `QueryClient` methods are synchronous (`fn`, not
   `async fn`). The production impl uses `runtime.block_on()` internally.
   This keeps the trait implementable for both sync mocks and async-backed
   production code.

3. **Graceful degradation.** When `storage` is `None`:
   - `list_panes` still works (WezTerm IPC only, no event counts/activity)
   - `list_events`, `search`, `mark_event_muted` return `DatabaseNotInitialized`
   - `list_triage_items` returns health items only, plus a DB-unavailable warning
   - `list_active_workflows`, `list_action_history`, `list_pane_bookmarks`,
     `list_saved_searches` return `Ok(Vec::new())`

4. **Constructor variants.** Four constructors for different initialization needs:
   - `new(layout)` - WezTerm only, no storage
   - `with_storage(layout, storage)` - Full capability
   - `with_wezterm(layout, wezterm)` - Custom WezTerm handle (testing)
   - `with_storage_and_wezterm(layout, storage, wezterm)` - Full custom

## View-to-Method Mapping

| View | Methods Called | Refresh Trigger |
|------|--------------|----------------|
| Home | `health`, `list_panes` (via health), `list_active_workflows` | Timer, `r` key |
| Panes | `list_panes`, `list_pane_bookmarks` | Timer, `r` key |
| Events | `list_events` | Timer, `r` key, filter change |
| Triage | `list_triage_items` | Timer, `r` key |
| History | `list_action_history` | Timer, `r` key, filter change |
| Search | `search`, `list_saved_searches` | `Enter` key (execute), `Ctrl+r` (run saved) |
| Help | (none) | Static |

## Testing Contract

### MockQueryClient Requirements

Any mock implementation must:

1. Implement all 8 required methods (the others have defaults).
2. Return deterministic data (no system clock, no network, no real WezTerm).
3. Support configurable empty/populated states for testing edge cases.

The existing `MockQueryClient` in `query.rs:878-958` demonstrates the minimum
viable mock. It covers `list_panes`, `list_events`, `list_triage_items`,
`search`, `health`, `is_watcher_running`, `mark_event_muted`, and
`list_active_workflows`.

### ftui Mock Extension

For ftui view testing, the mock should additionally support:
- `list_action_history` (History view tests)
- `list_pane_bookmarks` (Panes view bookmark indicator tests)
- `list_saved_searches` (Search view saved search navigation tests)
- `ruleset_profile_state` (Home view profile display tests)

## Stability Rules

1. **No new required methods.** New `QueryClient` methods must provide a default
   implementation that returns a reasonable empty/default value. This prevents
   breaking existing implementations (including mocks and tests).

2. **No method removal.** Existing methods are permanent until the post-migration
   cleanup phase (FTUI-09). Even then, removal requires a deprecation cycle.

3. **Response types are append-only.** New fields may be added to view types
   with default values. No existing field may be removed or change type.

4. **Error variants are append-only.** New `QueryError` variants may be added.
   Existing variants must not be removed or change semantics.

5. **`ui_query.rs` types stay framework-agnostic.** Shared types in `ui_query.rs`
   must not import ratatui, crossterm, ftui, or any rendering framework types.
   They use only `std`, `serde`, and wa-core internal types.

## References

- QueryClient source: `crates/wa-core/src/tui/query.rs`
- Shared UI types: `crates/wa-core/src/ui_query.rs`
- ADR-0005: Architecture Ring Map (QueryClient is Ring 3 / Ring 2 boundary)
- ADR-0006: Parity Contract (data flow section)
- Downstream: wa-1utb (FTUI-02.1), FTUI-05.* (view migration)
