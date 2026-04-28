# ARS Evidence Ledger Profiling Ledger

Bead: `ft-f6mu7`

## Scenario

Measure ARS evidence-ledger scaling before proposing workflow-forensics
optimizations. The Criterion harness is
`crates/frankenterm-core/benches/ars_evidence_ledger.rs`.

Workloads:

| Operation | Scales | Description |
| --- | --- | --- |
| `append` | 1k, 10k, 100k entries | Build a hash-chained `EvidenceLedger` with synthetic workflow evidence payloads. |
| `verify` | 1k, 10k, 100k entries | Recompute and validate every hash-chain link. |
| `serialize_json` | 1k, 10k, 100k entries | Serialize the full ledger to JSON bytes with `serde_json::to_vec`. |

The synthetic entries rotate across the shipped ARS evidence categories and
include representative workflow, pane, confidence, risk, approval, and signal
payload fields.

## Baseline

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench ars_evidence_ledger -- --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 --noplot
```

Median latencies from the local run:

| Operation | 1k entries | 10k entries | 100k entries |
| --- | ---: | ---: | ---: |
| `append` | `1.7478 ms` | `17.7564 ms` | `182.7267 ms` |
| `verify` | `1.1508 ms` | `11.6232 ms` | `117.4511 ms` |
| `serialize_json` | `1.6820 ms` | `17.6104 ms` | `176.9206 ms` |

## Hypothesis Ledger

| Hypothesis | Status | Evidence |
| --- | --- | --- |
| `append-scales-linearly` | supports | Median append time rises from `1.7478 ms` at 1k to `17.7564 ms` at 10k and `182.7267 ms` at 100k. |
| `verify-is-cheaper-than-append` | supports | Median verify time is lower than append at every measured scale: `1.1508 ms`, `11.6232 ms`, and `117.4511 ms`. |
| `json-serialization-cost-is-append-class` | supports | Serialization roughly tracks append cost at all scales; at 100k entries it measured `176.9206 ms` versus append `182.7267 ms`. |
| `payload-allocation-dominates-append` | pending | Append includes synthetic payload construction plus entry hash computation; allocator profiling is needed to split them. |
| `hash-recompute-dominates-verify` | pending | Verify recomputes entry hashes over already-built payloads; a sampler is needed to separate hashing from canonical payload serialization inside `compute_entry_hash`. |

## Artifacts

- `tests/artifacts/perf/ars-evidence-ledger-ft-f6mu7/fingerprint.json`
- `tests/artifacts/perf/ars-evidence-ledger-ft-f6mu7/hotspots.md`

Notes:

- Criterion emitted short-window warnings for the 100k-entry cases and
  extended the collection time automatically.
- Existing `frankenterm-core` warnings were present during compile; no new
  benchmark compile error was introduced.
