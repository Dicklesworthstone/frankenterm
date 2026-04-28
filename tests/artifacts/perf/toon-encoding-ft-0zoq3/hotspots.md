# TOON Encoding Hotspot Table

Run ID: `toon-encoding-ft-0zoq3`

| Rank | Location | Metric | Value | Category | Evidence |
| --- | --- | --- | --- | --- | --- |
| 1 | `toon_encoding_wa_search/toon_decode/1000` | p50 latency | `5.8549 ms` | decode CPU | local Criterion output, `fingerprint.json` command |
| 2 | `toon_encoding_wa_events/toon_decode/1000` | p50 latency | `5.8887 ms` | decode CPU | local Criterion output, `fingerprint.json` command |
| 3 | `toon_encoding_wa_search/toon_encode/1000` | p50 latency | `4.7127 ms` | encode CPU | local Criterion output, `fingerprint.json` command |
| 4 | `toon_encoding_wa_events/toon_encode/1000` | p50 latency | `4.5619 ms` | encode CPU | local Criterion output, `fingerprint.json` command |
| 5 | `toon_encoding_wa_state/toon_decode/200` | p50 latency | `1.3805 ms` | decode CPU | local Criterion output, `fingerprint.json` command |
| 6 | `toon_encoding_wa_state/toon_encode/200` | p50 latency | `1.0472 ms` | encode CPU | local Criterion output, `fingerprint.json` command |

Comparison anchors from the same run:

| Workload | JSON encode p50 | TOON encode p50 | TOON decode p50 |
| --- | ---: | ---: | ---: |
| `wa.state/200` | `218.54 us` | `1.0472 ms` | `1.3805 ms` |
| `wa.search/1000` | `771.87 us` | `4.7127 ms` | `5.8549 ms` |
| `wa.events/1000` | `850.25 us` | `4.5619 ms` | `5.8887 ms` |
