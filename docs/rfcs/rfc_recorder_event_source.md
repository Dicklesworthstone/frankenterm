# RFC: RecorderEventSource/RecorderEventCursor Trait Pattern

## Status
Proposed

## Problem
FrankenTerm's flight recorder system stores terminal events (ingress, egress, control, lifecycle) through `RecorderStorage`. When migrating between storage backends (AppendLog → SQLite), consumers need backend-neutral read access to event streams without coupling to storage implementation details.

## Solution
Define two traits that decouple event reading from storage:

### RecorderEventSource
```rust
trait RecorderEventSource: Send + Sync {
    fn event_count(&self) -> u64;
    fn pane_ids(&self) -> Vec<u64>;
    fn schema_version(&self) -> &str;
    fn open_cursor(&self, pane_id: Option<u64>) -> Box<dyn RecorderEventCursor>;
}
```

### RecorderEventCursor
```rust
trait RecorderEventCursor {
    fn next_batch(&mut self, max: usize) -> Vec<RecorderEvent>;
    fn seek_to_offset(&mut self, offset: u64);
    fn current_offset(&self) -> u64;
    fn remaining_estimate(&self) -> u64;
}
```

### Key Properties
- **Object safety**: Both traits are object-safe (no async, no generics in methods).
- **Batch-oriented**: `next_batch` amortizes overhead vs per-event iteration.
- **Filterable**: `open_cursor` accepts optional pane_id for per-pane scanning.
- **Resumable**: `seek_to_offset` enables checkpoint-based resumption.

## Code Example
```rust
fn verify_migration(source: &dyn RecorderEventSource, target: &dyn RecorderEventSource) -> bool {
    source.event_count() == target.event_count()
        && source.pane_ids() == target.pane_ids()
}
```

## Testing Guidance
- Verify trait is object-safe: `let _: Box<dyn RecorderEventSource>;`
- Test cursor batch semantics: empty source → empty batch
- Test seek + resume: seek to middle, verify offset
- Property test: total events via cursor == event_count()
