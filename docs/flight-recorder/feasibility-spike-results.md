# Feasibility Spike Results (`ft-oegrb.1.6`)

Date: 2026-02-12  
Author: `WildSpring`

## Purpose
Run early feasibility spikes for the three hottest paths in the proposed recorder architecture:
1. append writer throughput behavior
2. lexical indexing/search runtime envelope (proxy using cass Tantivy benches)
3. hybrid rank-fusion compute cost scaling

These are pre-implementation spikes to de-risk architecture direction, not final production benchmarks.

## Spike A: Append Writer Throughput (Synthetic)

### Method
- Environment: local machine, temporary file in `/tmp`
- Record format: `u32 length prefix + payload`
- Writer model:
  - batched writes (`batch` records/write syscall)
  - periodic `fsync` (`fsync_every_batches`)
- Command used: inline Python append loop with explicit `os.write` and `os.fsync`

### Results
| events target | payload bytes | batch | fsync cadence | committed events | elapsed | events/sec | MiB/sec |
|---|---|---|---|---|---|---|---|
| 200,000 | 256 | 256 | every 8 batches | 199,936 | 0.015s | 12.9M | 3200.9 |
| 200,000 | 256 | 1024 | every 4 batches | 199,680 | 0.011s | 17.9M | 4428.6 |
| 500,000 | 192 | 1024 | every 8 batches | 499,712 | 0.019s | 25.6M | 4790.0 |

### Interpretation
- Raw append throughput is clearly not a bottleneck on this machine for recorder-sized events.
- Batching materially improves throughput.
- These numbers are optimistic due to cache effects and short run duration; they still confirm feasibility of append-first architecture.

## Spike B: Lexical Index/Search Runtime (Tantivy Proxy via cass)

### Method
- Reused existing criterion benchmark harness from cass:
  - repo: `/Users/jemanuel/projects/coding_agent_session_search`
  - bench target: `benches/runtime_perf.rs`
- Commands run:
  - `cargo bench --bench runtime_perf index_small_batch -- --noplot`
  - `cargo bench --bench runtime_perf search_latency -- --noplot`

### Results
- `index_small_batch`: `182.35 ms .. 203.10 ms` (criterion range)
- `search_latency`: `10.081 µs .. 10.340 µs`

### Interpretation
- Tantivy-based lexical projection and query latency are easily in practical range for our target architecture.
- This is an external proxy benchmark (cass schema/workload), not yet recorder-native data; still strong evidence for path viability.

## Spike C: Hybrid RRF Fusion Cost Scaling (Microbench)

### Method
- Implemented deterministic RRF fusion in Python for quick complexity spike.
- Two ranked lists fused (`lexical` + `semantic`) with `k=60`.
- 30-run average per case.

### Results
| lexical candidates | semantic candidates | fused docs | avg fusion time |
|---|---|---|---|
| 200 | 200 | 338 | 0.053 ms |
| 1,000 | 1,000 | 1,687 | 0.264 ms |
| 5,000 | 5,000 | 8,353 | 1.462 ms |
| 10,000 | 10,000 | 16,696 | 3.126 ms |

### Interpretation
- Hybrid RRF fusion cost remains low even at large candidate counts.
- Expected bottleneck is candidate generation/index IO, not fusion arithmetic.

## Go / No-Go Conclusions

### Writer path
- **Go**: append-only recorder log with batch writes is feasible.
- Implementation focus should be correctness/recovery/backpressure, not raw write speed.

### Indexing path
- **Go**: Tantivy lexical index pipeline is feasible for target use.
- Must validate with recorder-native event corpus once implementation exists.

### Hybrid path
- **Go**: RRF fusion overhead is negligible at expected candidate sizes.
- Safe to keep deterministic RRF as v1 hybrid baseline.

## Recommended Thresholds for Implementation Gates
- Writer:
  - sustained append throughput > expected ingest peak with 3x headroom
  - deterministic replay/checkpoint under restart
- Indexing:
  - bounded ingestion lag under burst load
  - lexical p95 query latency within interactive target
- Hybrid:
  - fusion step p95 < 5 ms for candidate sets up to 20k combined docs
  - lexical-safe fallback when semantic path unavailable

## Caveats
- Append spike reflects local filesystem cache behavior; durability-at-scale requires longer-run tests with realistic flush policies.
- Tantivy measurements came from cass harness, not FrankenTerm recorder schema.
- RRF spike used Python implementation for fast feasibility; final Rust implementation should be benchmarked in-repo.

## Next Action
Proceed from research track into implementation tracks:
- capture + canonical append path (`ft-oegrb.2.*`, `ft-oegrb.3.*`)
- recorder-native lexical benchmarking (`ft-oegrb.4.*`)
- in-repo hybrid benchmark harness (`ft-oegrb.5.*`, `ft-oegrb.7.*`)
