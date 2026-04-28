# TOON Encoding Profiling Ledger

Bead: `ft-0zoq3`

## Scenario

Measure the serialization cost of TOON robot/MCP envelopes before proposing
encoder optimizations. The Criterion harness is
`crates/frankenterm-core/benches/toon_encoding.rs`.

Workloads:

| Workload | Scales | Operations |
| --- | --- | --- |
| `wa.state` | 1, 10, 50, 200 panes | JSON encode, TOON encode, TOON decode |
| `wa.search` | 10, 100, 500, 1000 hits | JSON encode, TOON encode, TOON decode |
| `wa.events` | 10, 100, 500, 1000 events | JSON encode, TOON encode, TOON decode |

## Hypothesis Ledger

| Hypothesis | Status | Evidence |
| --- | --- | --- |
| `toon-encoder-cpu-dominates` | supports within serialization slice | Short Criterion run shows TOON encode at 200-pane `wa.state` p50 `1.0472 ms` versus JSON p50 `218.54 us`; 1000-hit `wa.search` p50 `4.7127 ms` versus JSON p50 `771.87 us`; 1000-event `wa.events` p50 `4.5619 ms` versus JSON p50 `850.25 us`. |
| `serde-json-faster-encode` | supports | JSON encode is faster than TOON encode at the largest scale for all three workloads in the short local run. |
| `allocation-per-cell` | pending | Requires allocator profile against the new benchmark or the live 200-pane workload. |
| `output-size-savings-real` | pending | Harness sets Criterion throughput from JSON/TOON byte lengths for each payload shape; size ratios should be extracted from the Criterion output in a dedicated reporting pass. |

## Profile Follow-Up

The benchmark is the baseline artifact needed before a sampler run. A follow-up
profile should capture:

1. `target/criterion/toon_encoding*/` Criterion output.
2. `tests/artifacts/perf/toon-encoding-<run-id>/fingerprint.json`.
3. A ranked hotspot table from the same host and build profile.

## Local Baseline

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench toon_encoding -- --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 --noplot
```

Artifact summary:

- `tests/artifacts/perf/toon-encoding-ft-0zoq3/fingerprint.json`
- `tests/artifacts/perf/toon-encoding-ft-0zoq3/hotspots.md`
