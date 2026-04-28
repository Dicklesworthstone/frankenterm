# Recorder Hot Path Hotspot Table

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench recorder_hot_path -- --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 --noplot
```

| Rank | Location | Metric | Value | Category | Evidence |
| --- | --- | --- | --- | --- | --- |
| 1 | `record_event_with_cx` + per-event `FrameWriter::flush` | median per 100-event batch | `1.2866 ms` | CPU/I/O | `/tmp/ft-jemanuel-local-target/criterion/recorder_hot_path/event_flush_each_100eps_10panes/new/estimates.json` |
| 2 | `record_event_with_cx` + buffered recorder stop flush | median per 100-event batch | `1.0141 ms` | CPU/I/O | `/tmp/ft-jemanuel-local-target/criterion/recorder_hot_path/event_buffered_100eps_10panes/new/estimates.json` |

Derived per-event medians:

| Case | Median per 100 events | Approx median per event | Throughput from Criterion |
| --- | --- | --- | --- |
| Flush each event | `1.2866 ms` | `12.87 us/event` | `71.384-76.600 Kelem/s` |
| Buffered | `1.0141 ms` | `10.14 us/event` | `96.022-98.933 Kelem/s` |

Hypothesis status:

| Hypothesis | Verdict | Evidence |
| --- | --- | --- |
| `flush-each-event-costs-more` | supports | Flush-each median is about `27%` slower than buffered in the short local run. |
| `record-event-hot-path-is-sub-ms-per-event` | supports | Both cases are well under `1 ms/event` for the synthetic 10-pane, 100 events/sec scenario. |
| `redaction-or-json-dominates` | pending | The current harness intentionally leaves default event redaction on, so redaction and JSON frame encoding are not yet separated. |
