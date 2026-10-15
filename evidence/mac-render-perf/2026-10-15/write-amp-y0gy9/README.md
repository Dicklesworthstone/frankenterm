# ft-y0gy9 / ft-yccm0.2.1.3: native write amplification at 76fe16cc6

Native, 2026-10-15, mac-studio (M5 Ultra, macOS 27, APFS, load 22-26). The
mux server is the one in the release-interactive bundle built from 76fe16cc6.
The probe is `scripts/measure-mux-write-amplification.sh <bundle>/Contents/MacOS`.
It runs a private mux with an isolated HOME, no clients and the default
config, and compares the server's own `proc_pid_rusage` disk writes with
the bytes its one pane printed. These are byte counts, not timings, so host
load does not bias them.

| pane command | emitted | written | amplification | target | 2026-09-26 |
|---|---|---|---|---|---|
| `seq 1 2000000` (`wa-seq.txt`, `wa-seq2.txt`) | 18.78 MB | 87.41 / 87.40 MB | 4.66x / 4.65x | <= 5x | 71.8x |
| 200k x 91-byte lines (`wa-91.txt`, `wa-912.txt`) | 18.20 MB | 38.47 / 38.46 MB | 2.11x / 2.11x | <= 3x | 21.4x |

The workspace grew by about what was written (18.5 MB and 37.9 MB). The WAL
copy and the compaction rewrites are gone. What remains is the retained,
sealed rows.
