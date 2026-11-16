# Recorder Hot Path Profiling Ledger

Bead: `ft-9r82k`

## Scenario

Measure recorder detection-frame write cost before proposing recorder hot-path
optimizations. The Criterion harness is
`crates/frankenterm-core/benches/recorder_hot_path.rs`.

Workload:

| Workload | Shape | Operations |
| --- | --- | --- |
| `event_flush_each_100eps_10panes` | 100 events/sec across 10 active panes | `record_event_with_cx` with `flush_threshold=1`, then stop/flush all recorders |
| `event_buffered_100eps_10panes` | 100 events/sec across 10 active panes | `record_event_with_cx` with default 64-frame buffering, then stop/flush all recorders |

The harness uses redaction enabled for event payloads because that is the
default `RecordingOptions` behavior.

## Hypothesis Ledger

| Hypothesis | Status | Evidence |
| --- | --- | --- |
| `flush-each-event-costs-more` | supports | Short local Criterion run measured `event_flush_each_100eps_10panes` median `1.2866 ms` per 100-event batch versus `event_buffered_100eps_10panes` median `1.0141 ms`. |
| `record-event-hot-path-is-sub-ms-per-event` | supports | Both measured medians are about `10.1-12.9 us/event` for the 100-event batch on the local M4 Pro run. |
| `redaction-or-json-dominates` | pending | The benchmark exercises redaction and JSON event-frame serialization together; attribution needs a follow-up profile or a no-redaction comparison. |
| `filesystem-sync-dominates` | pending | The flush-each-event case calls buffered file flushes, but not `sync_data`; durable-media attribution needs a separate fsync benchmark if the production path starts requiring it. |

## Local Baseline

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench recorder_hot_path -- --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 --noplot
```

Artifact summary:

- `tests/artifacts/perf/recorder-hot-path-ft-9r82k/fingerprint.json`
- `tests/artifacts/perf/recorder-hot-path-ft-9r82k/hotspots.md`

Notes:

- Criterion requested 20 samples from the harness group despite the command
  line's `--sample-size 10`; both benchmark cases completed and reported
  sample-time warnings because the requested measurement window was short.
- Existing `frankenterm-core` warnings were present during compile; no new
  benchmark compile error was introduced.
