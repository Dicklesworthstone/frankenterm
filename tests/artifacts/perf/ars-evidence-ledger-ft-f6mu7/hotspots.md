# ARS Evidence Ledger Hotspot Table

Command:

```text
scripts/cargo-local.sh bench -p frankenterm-core --bench ars_evidence_ledger -- --sample-size 10 --warm-up-time 0.1 --measurement-time 0.2 --noplot
```

| Rank | Location | Metric | Value | Category | Evidence |
| --- | --- | --- | --- | --- | --- |
| 1 | `ars_evidence_ledger/append/100000` | median per 100k-entry build | `182.7267 ms` | CPU/alloc | `/tmp/ft-jemanuel-local-target/criterion/ars_evidence_ledger/append/100000/new/estimates.json` |
| 2 | `ars_evidence_ledger/serialize_json/100000` | median full-ledger JSON serialization | `176.9206 ms` | CPU/alloc | `/tmp/ft-jemanuel-local-target/criterion/ars_evidence_ledger/serialize_json/100000/new/estimates.json` |
| 3 | `ars_evidence_ledger/verify/100000` | median full-chain verification | `117.4511 ms` | CPU | `/tmp/ft-jemanuel-local-target/criterion/ars_evidence_ledger/verify/100000/new/estimates.json` |
| 4 | `ars_evidence_ledger/append/10000` | median per 10k-entry build | `17.7564 ms` | CPU/alloc | `/tmp/ft-jemanuel-local-target/criterion/ars_evidence_ledger/append/10000/new/estimates.json` |
| 5 | `ars_evidence_ledger/serialize_json/10000` | median full-ledger JSON serialization | `17.6104 ms` | CPU/alloc | `/tmp/ft-jemanuel-local-target/criterion/ars_evidence_ledger/serialize_json/10000/new/estimates.json` |

Full median table:

| Operation | 1k entries | 10k entries | 100k entries |
| --- | ---: | ---: | ---: |
| `append` | `1.7478 ms` | `17.7564 ms` | `182.7267 ms` |
| `verify` | `1.1508 ms` | `11.6232 ms` | `117.4511 ms` |
| `serialize_json` | `1.6820 ms` | `17.6104 ms` | `176.9206 ms` |

Hypothesis status:

| Hypothesis | Verdict | Evidence |
| --- | --- | --- |
| `append-scales-linearly` | supports | 10x entry growth produces roughly 10x median growth through 100k entries. |
| `verify-is-cheaper-than-append` | supports | Verify is about `64%` of append time at 100k entries in this run. |
| `json-serialization-cost-is-append-class` | supports | Serialization is within `4%` of append time at 100k entries. |
