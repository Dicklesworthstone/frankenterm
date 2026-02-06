# Backpressure Policy Design

> Design specification for bead `wa-upg.12.1`. Provides deterministic,
> implementable backpressure signals, thresholds, and degradation responses
> for the wa watcher pipeline.

---

## Problem Statement

The wa watcher pipeline has three bounded queues in series:

```
Tailers ─► capture channel (1024) ─► persistence task ─► write queue (10000) ─► SQLite
                                         │
                                         ▼
                                    pattern engine ─► event bus (1000 per channel)
```

When any downstream stage stalls (disk I/O spike, slow FTS5 indexing, lock
contention), upstream queues fill. Without explicit backpressure the failure
mode is silent data loss: tailers hit `send_timeout`, increment a counter,
and after 5 consecutive failures emit a gap segment. The gap is the only
observable signal that output was missed.

This design replaces ad-hoc gap emission with a **tiered, deterministic
backpressure policy** that adapts the pipeline before data is lost.

---

## Existing Infrastructure

### Queue Depth Signals (already available)

| Queue | Type | Default Size | Depth API |
|-------|------|-------------|-----------|
| Capture channel | `mpsc::Sender<CaptureEvent>` | 1,024 | `max_capacity() - capacity()` |
| Write queue | `mpsc::Sender<WriteCommand>` | 10,000 | `StorageHandle::write_queue_depth()` |
| Event bus (broadcast) | `broadcast::Sender` | 1,000 | subscriber lag via `lagged()` |

### Existing Constants

| Constant | Value | Location |
|----------|-------|----------|
| `OVERFLOW_BACKPRESSURE_THRESHOLD` | 5 | `tailer.rs` |
| `BACKPRESSURE_WARN_RATIO` | 0.75 | `runtime.rs` |
| Default poll interval | 200 ms | `IngestConfig` |
| Min poll interval | 50 ms | `IngestConfig` |
| `backpressure_threshold` (unused) | 1,000 | `IngestConfig` |

### Existing Degradation System

`degradation.rs` provides per-subsystem `Normal/Degraded/Unavailable` states
with recovery tracking. The backpressure policy extends this by adding a
**load-responsive tier** that acts before subsystems fully degrade.

---

## Tier Model

Four tiers, ordered by severity:

| Tier | Condition | Response |
|------|-----------|----------|
| **Green** | All queues below warning thresholds | Normal operation |
| **Yellow** | Capture ≥ 50% OR write ≥ 60% | Slow down, defer non-essential work |
| **Red** | Capture ≥ 75% OR write ≥ 80% | Pause low-priority panes, shed load |
| **Black** | Queue near saturation (within 5 of capacity) | Emergency: essential panes only |

### Tier Thresholds (configurable)

```toml
[backpressure]
enabled = true
check_interval_ms = 500

# Capture channel thresholds (fraction of capacity)
yellow_capture = 0.50
red_capture = 0.75

# Write queue thresholds (fraction of capacity)
yellow_write = 0.60
red_write = 0.80

# Hysteresis: stay in elevated tier for at least this long before downgrading
hysteresis_ms = 2000
```

### Transition Rules

1. **Upgrade** (toward Black): immediate on threshold breach.
2. **Downgrade** (toward Green): delayed by `hysteresis_ms` to prevent
   oscillation.
3. **Classification** uses the *worse* of capture and write ratios—if either
   queue is in Red territory, the system is Red.
4. Tier is sampled every `check_interval_ms` (default 500 ms) in the
   maintenance task.

---

## Response Actions

### Yellow Tier

| Action | Mechanism |
|--------|-----------|
| Extend idle poll backoff | Multiply idle pane backoff by `idle_poll_backoff_factor` (default 2.0) |
| Defer FTS5 indexing | Set flag; persistence task skips FTS insert triggers, batches for later |
| Sample pattern detection | Skip detection for lowest `skip_detection_ratio` (default 0.25) of panes by priority |
| Emit health warning | Log at `warn` level; include in `HealthSnapshot` |

### Red Tier

| Action | Mechanism |
|--------|-----------|
| Pause low-priority panes | `TailerSupervisor` skips lowest `pause_ratio` (default 0.50) of panes |
| Emit gap for paused panes | `backpressure_pause` gap recorded per paused pane |
| Skip detection except high-priority | Only run pattern engine for panes with priority ≤ 50 |
| Force checkpoint | Request `PRAGMA wal_checkpoint(PASSIVE)` |
| Cap persistence buffer | Drop oldest buffered segments when in-memory count exceeds `max_buffered_segments` |

### Black Tier

All Red actions plus:

| Action | Mechanism |
|--------|-----------|
| Essential-only capture | Only highest-priority pane per agent type is polled |
| Drop non-essential captures | Immediate gap emission; no persistence attempted |
| Trigger degradation system | `enter_degraded(DbWrite, "backpressure_black")` |

### Recovery

When queues drain below the current tier's thresholds (after hysteresis):

1. Resume pattern detection for all panes.
2. Re-enable FTS5 indexing; flush deferred batch.
3. Resume paused panes gradually (one every `recovery_resume_interval_ms`,
   default 500 ms) to avoid re-spiking queues.
4. Return to Green.

---

## Integration Points

### 1. Maintenance Task (`runtime.rs`)

Every `check_interval_ms`:

```
1. Sample capture_tx.max_capacity() - capture_tx.capacity()
2. Sample storage_handle.write_queue_depth()
3. Call backpressure_manager.evaluate(capture_depth, write_depth)
4. On tier change: log, update HealthSnapshot, apply actions
```

### 2. Tailer Supervisor

Read current tier before scheduling polls:

- **Yellow**: increase `max_interval` for idle panes.
- **Red/Black**: skip panes in `paused_panes` set.

### 3. Persistence Task

Read current tier before processing each segment:

- **Yellow**: conditionally skip FTS insert, skip detection for low-priority.
- **Red**: enforce `max_buffered_segments`.
- **Black**: drop segments from non-essential panes.

### 4. Health Snapshot

Add to `HealthSnapshot`:

```rust
pub backpressure_tier: String,
pub backpressure_paused_panes: Vec<u64>,
pub backpressure_deferred_fts: bool,
pub backpressure_detection_skip_ratio: f64,
```

---

## Pane Priority Interaction

Backpressure uses the existing `PanePriorityConfig` from `config.rs`:

- **Priority ≤ 50**: high (never paused, always detected).
- **Priority 51-100**: normal (paused in Red, skipped in Yellow for detection).
- **Priority > 100**: low (first to be paused/skipped).

Default priority is 100. Override via config:

```toml
[[ingest.priorities.rules]]
id = "critical_codex"
priority = 10
title = "codex"
```

---

## Gap Semantics

| Gap Reason | When Emitted |
|------------|-------------|
| `backpressure_pause` | Pane paused in Red/Black tier |
| `backpressure_overflow` | Segment dropped in Black tier |
| `backpressure_resume` | Pane resumed after downgrade (informational) |

Gap reasons are stable strings for downstream filtering.

---

## Observability

### Metrics

| Metric | Type |
|--------|------|
| `wa_backpressure_tier` | Gauge (0=Green, 1=Yellow, 2=Red, 3=Black) |
| `wa_backpressure_transitions_total` | Counter |
| `wa_backpressure_paused_panes` | Gauge |
| `wa_backpressure_segments_dropped_total` | Counter |
| `wa_backpressure_gaps_emitted_total` | Counter |
| `wa_backpressure_fts_deferred_total` | Counter |
| `wa_backpressure_detection_skipped_total` | Counter |

### Diagnostic Output

`wa status --health` (or `wa doctor`) includes:

```
Backpressure: YELLOW (12s)
  Capture queue:  520/1024 (50.8%)
  Write queue:    410/10000 (4.1%)
  Actions: idle backoff 2.0x, FTS deferred, detection sampled (skip 25%)
  Paused panes: none
```

---

## Testing Strategy

### Unit Tests

1. Tier classification: given (capture_depth, write_depth) → expected tier.
2. Hysteresis: verify downgrade delay.
3. Pane selection: verify priority-based pause ordering.
4. Recovery: verify gradual resume.

### Integration Tests

1. Simulate full capture channel → verify Yellow/Red transitions.
2. Simulate slow storage writes → verify write queue triggers.
3. Verify gap emission for paused panes.
4. Verify FTS deferred batch flush on recovery.

### E2E (covered by `wa-upg.12.6`)

1. Stress scenario with many panes + high output rate.
2. Verify graceful degradation (no panics, no silent data loss beyond gaps).
3. Artifact collection: tier transitions, gap counts, latency.

---

## Configuration Reference

```toml
[backpressure]
# Enable backpressure policy (default: true)
enabled = true

# How often to sample queue depths (ms)
check_interval_ms = 500

# Capture channel thresholds (fraction of max_capacity)
yellow_capture = 0.50
red_capture = 0.75

# Write queue thresholds (fraction of writer_queue_size)
yellow_write = 0.60
red_write = 0.80

# Minimum time in elevated tier before downgrading (ms)
hysteresis_ms = 2000

# Yellow tier: multiply idle pane poll interval by this factor
idle_poll_backoff_factor = 2.0

# Yellow tier: skip pattern detection for this fraction of lowest-priority panes
skip_detection_ratio = 0.25

# Red tier: pause ingest for this fraction of lowest-priority panes
pause_ratio = 0.50

# Red tier: maximum segments buffered in persistence task before dropping
max_buffered_segments = 100

# Recovery: resume one paused pane every N ms
recovery_resume_interval_ms = 500
```

---

## Acceptance Criteria

- [ ] Policy is deterministic: same inputs produce same tier classification.
- [ ] Tier transitions are logged and visible in health snapshots.
- [ ] Paused panes emit explicit gap segments.
- [ ] Recovery is gradual (no re-spike).
- [ ] Configuration is optional with sensible defaults.
- [ ] No silent data loss beyond explicitly recorded gaps.
