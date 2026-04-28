# WAL Checkpoint Sustained Write Profiling Ledger

Bead: `ft-ctt7k`

## Scenario

Measure SQLite WAL checkpoint cost after sustained capture/storage writes before
changing checkpoint policy. The Criterion harness is
`crates/frankenterm-core/benches/wal_checkpoint_sustained_write.rs`.

Workload:

| Workload | Shape | Operations |
| --- | --- | --- |
| `write_10k_events_checkpoint` | 10,000 pattern events across 10 panes with logical 100 events/sec timestamps | `StorageHandle::record_event_with_cx` for every event, then `StorageHandle::checkpoint_with_cx` on the writer thread |

The benchmark models a sustained 100/sec stream by assigning event timestamps at
10ms intervals. It intentionally does not sleep for the full 100-second logical
duration, so the harness remains practical for local regression runs.

## Metrics Captured

The bench emits the normal Criterion timing plus a JSON summary artifact:

```text
crates/frankenterm-core/target/criterion/wal-checkpoint-ft-ctt7k-summary.json
```

Summary fields:

| Field | Meaning |
| --- | --- |
| `steady_state_p99_write_us` | p99 per-event `record_event_with_cx` latency after skipping the first logical second |
| `max_write_us` | worst single event write in the 10k-event run |
| `checkpoint_pause_us` | wall-clock pause for `checkpoint_with_cx` after the 10k writes |
| `wal_pages` | WAL pages reported by the storage checkpoint result |
| `checkpoint_vs_p99_ratio` | checkpoint pause divided by steady-state p99 write latency |

## Local Baseline

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench wal_checkpoint_sustained_write
```

Environment from the emitted bench manifest:

| Field | Value |
| --- | --- |
| OS | `macos` |
| Arch | `aarch64` |
| CPU | `Apple M4 Pro` |
| Rust | `1.97.0-nightly` |

Criterion result:

| Metric | Value |
| --- | --- |
| Workload time | `[558.36 ms, 572.49 ms, 589.63 ms]` |
| Throughput | `[16.960 Kelem/s, 17.468 Kelem/s, 17.910 Kelem/s]` |

Internal workload summary from
`crates/frankenterm-core/target/criterion/wal-checkpoint-ft-ctt7k-summary.json`:

| Metric | Value |
| --- | ---: |
| Events written | 10,000 |
| Logical event rate | 100/sec |
| Panes | 10 |
| Steady-state p99 event write | 167 us |
| Max event write | 12,262 us |
| WAL checkpoint pause | 1,790 us |
| WAL pages checkpointed | 426 |
| Checkpoint / p99 write ratio | 10.72x |

## Hypothesis Ledger

| Hypothesis | Status | Evidence |
| --- | --- | --- |
| `checkpoint-pause-is-visible-but-sub-10ms` | supports | Local baseline measured a 1.790ms checkpoint pause after 10k event writes. |
| `steady-state-event-write-tail-is-sub-ms` | supports | Steady-state p99 write latency was 167us after skipping the first logical second. |
| `single-write-outliers-exist` | supports | Max single event write reached 12.262ms while p99 stayed 167us, so occasional outliers need separate attribution before tuning checkpoint policy. |
| `checkpoint-cost-dominates-p99-write` | supports | Checkpoint pause was 10.72x the steady-state p99 event write latency. |

## Notes

- The checkpoint is measured through `StorageHandle::checkpoint_with_cx`, so it
  includes the production writer-thread enqueue/response path plus
  `PRAGMA wal_checkpoint(PASSIVE)` and `PRAGMA optimize`.
- The benchmark records warning-heavy `frankenterm-core` compilation output from
  existing code, but the bench itself compiled and ran successfully.
- The harness captures a baseline; it does not change checkpoint policy,
  `wal_autocheckpoint`, batching, or SQLite pragmas.
