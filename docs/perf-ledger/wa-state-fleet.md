# wa.state at fleet scale — perf ledger (ft-3r0n4)

## Scope

Full envelope-construction pipeline an `ft robot state` / MCP `wa.state` call traverses, measured at 10 / 50 / 200 panes across two output formats (JSON + TOON). Differentiated from the `toon_encoding` bench (ft-0zoq3) by including the redactor sweep + envelope wrapping cost split.

## Pipelines benched

| Pipeline | Steps included |
|----------|----------------|
| `wa_state_fleet_construct_only` | Build `Vec<Value>` only |
| `wa_state_fleet_construct_redact` | Construct + redactor sweep on title/cwd/ignore_reason |
| `wa_state_fleet_full_envelope_json` | Construct + redact + envelope wrap + JSON encode |
| `wa_state_fleet_full_envelope_toon` | Construct + redact + envelope wrap + TOON encode |

Subtraction of group medians attributes:
- `redactor_cost = construct_redact - construct_only`
- `serialize_json = full_envelope_json - construct_redact`
- `serialize_toon = full_envelope_toon - construct_redact`

Half the synthetic panes carry an `OPENAI_API_KEY=sk-fake…` fragment in `cwd` so the redactor regex actually fires (not a no-op measurement).

## Hypothesis ledger

Pre-measurement predictions, to be falsified or supported by criterion output. The skill mandates writing these BEFORE the bench runs so confirmation bias doesn't bend the analysis.

| ID | Hypothesis | Verdict |
|----|------------|---------|
| H1 | Construct-only scales linearly: 10p ≈ k, 50p ≈ 5k, 200p ≈ 20k. The synthetic builder is `Vec::with_capacity` + 200 `json!{…}` invocations; no algorithmic surprise. | pending |
| H2 | Redactor cost dominates serialization at 200 panes. Each pane runs the full regex set against `title + cwd + ignore_reason`; eight regexes × 200 panes = 1600 regex evaluations vs ~1 serialization pass. | pending |
| H3 | TOON encode is 1.5–2× SLOWER than JSON at fleet scale. JSON has highly-tuned `serde_json::to_vec`; TOON's encoder is younger, less optimized, with table-rendering overhead per object. | pending |
| H4 | Output bytes: TOON is 30–40% smaller than JSON at fleet scale. README sells TOON as token-efficient; the bench's `Throughput::Bytes` reports the compressed-form delta. | pending |
| H5 | Super-linear scaling between 50p and 200p in `full_envelope_*` is from `Value`'s internal `BTreeMap` ordering work growing with envelope size. | pending |

## Methodology

```
cargo bench -p frankenterm-core --bench wa_state_fleet 2>&1 | tee \
  tests/artifacts/perf/wa-state-fleet-$(git rev-parse --short HEAD).log
```

Compare against the criterion baseline saved by previous runs:

```
cargo bench -p frankenterm-core --bench wa_state_fleet -- --save-baseline ft-3r0n4
# … later:
cargo bench -p frankenterm-core --bench wa_state_fleet -- --baseline ft-3r0n4
```

CI integration: `scripts/check_bench_budgets.sh` reads `target/criterion/wa-budgets.json` and fails the build if any group's median exceeds the `bench_common` threshold table.

## Measured (CI fills this in)

Numbers below are placeholders — the table will be populated by the first CI run. Each value is criterion's reported median; throughput shows bytes/sec for the JSON+TOON groups, elements/sec for construct.

```
| Pipeline                 | 10 panes  | 50 panes   | 200 panes  | scale 200/10 |
|--------------------------|-----------|------------|------------|--------------|
| construct_only           | _ µs      | _ µs       | _ µs       | _ ×          |
| construct_redact         | _ µs      | _ µs       | _ µs       | _ ×          |
| full_envelope_json       | _ µs      | _ µs       | _ µs       | _ ×          |
| full_envelope_toon       | _ µs      | _ µs       | _ µs       | _ ×          |
| redactor_cost  (derived) | _ µs      | _ µs       | _ µs       | —            |
| serialize_json (derived) | _ µs      | _ µs       | _ µs       | —            |
| serialize_toon (derived) | _ µs      | _ µs       | _ µs       | —            |
```

The fingerprint header (per the profiling skill's contract) goes alongside each populated row: CPU model + cores + governor + kernel + toolchain + LTO mode + same-host validation.

## Hand-off

Per the profiling skill: this bead stops at the hotspot table. If H2 (redactor dominates) is supported, the optimization (precompiled regex set sharing, batched scan, replace `Vec<Value>` walk with typed struct + scrub-on-construct) becomes a follow-on bead routed to `/extreme-software-optimization`.

If H3 is supported (TOON 1.5–2× slower), the TOON encoder optimization is the same hand-off — but routed to `toon_rust` upstream rather than ft itself.

If both H2 and H3 are rejected (e.g., cost is dominated by `Value` cloning during `iter_batched`'s setup), the optimization is on the bench harness rather than the production path — and ft is fine.

## References

- `crates/frankenterm-core/benches/wa_state_fleet.rs` — the bench
- `crates/frankenterm-core/src/mcp_tools.rs:1253` — production `redact_mcp_pane_state_fields`
- `crates/frankenterm-core/src/redactor.rs` — the regex set under measurement
- `crates/frankenterm-core/benches/toon_encoding.rs` (ft-0zoq3) — sibling bench, encoder-only
- `docs/perf-ledger/toon-encoding.md` — sibling perf ledger
