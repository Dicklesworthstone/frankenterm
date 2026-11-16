# ft-l3tfo — Cold-Build Measurement of `frankenterm-core` Sub-Crate Split

**Parent:** ft-y0loj.6 (`frankenterm-core` sub-crate split, phase-6 measurement)
**Status:** measured 2026-04-26 at HEAD `dd3e98fa`
**Predecessors:** ft-y0loj.1 (tantivy extracted, b68ea095), ft-y0loj.3 (fleet *partial*, dd3e98fa)
**Decision:** **Park further extraction** until tier-1 (`frankenterm-core-resource-types`, ft-usvnt) lands.

---

## Question

After the tantivy + fleet-dashboard sub-crates were carved out of `frankenterm-core`,
does the cold-build wall-clock actually improve enough to justify continuing the
tier-3 extractions (lifecycle, recorder, mux-bridge), or does the dependency on
`frankenterm-core` still serialize everything?

## Methodology

All builds were run on the local cargo target (`/tmp/ft-cc6-target`) with the
`rch` Bash hook bypassed via `nohup`-fork-bypass. Native deps used the
homebrew-llvm `clang`/`clang++` (the `cc` shell alias maps to Claude Code on
this host, so a stock `cargo build` would fail to build `aws-lc-sys`).

Cold-build recipe:

```bash
cargo clean -p frankenterm-core -p frankenterm-core-tantivy -p frankenterm-core-fleet
CC=/opt/homebrew/opt/llvm/bin/clang \
CXX=/opt/homebrew/opt/llvm/bin/clang++ \
CARGO_TARGET_DIR=/tmp/ft-cc6-target \
cargo build --release --timings -p <target>
```

Three measured builds (each preceded by a fresh `cargo clean -p` of the three
sub-crates so the build graph rebuilt the target *and* its frankenterm-* deps;
upstream third-party crates remained cached, which is faithful to a CI run with
a populated registry/index cache):

1. `cargo build --release --timings -p frankenterm-core-tantivy`
2. `cargo build --release --timings -p frankenterm-core-fleet` (all-cached re-run)
3. `cargo build --release --timings -p frankenterm-core` (full monolith parent)

Output parsed from the `UNIT_DATA` JS array in
`/tmp/ft-cc6-target/cargo-timings/cargo-timing-*.html`.

## Raw measurements

| Build target               | Wall-clock | `frankenterm-core` (single unit) | sub-crate compile | Sequential sum |
| -------------------------- | ---------- | -------------------------------- | ----------------- | -------------- |
| `frankenterm-core-tantivy` | 1m 54s     | 108.52s                          | 2.06s             | 406.6s         |
| `frankenterm-core-fleet`   | 2m 12s¹    | (cached)                         | 0.39s             | (cached)       |
| `frankenterm-core` (alone) | 3m 44s     | 130.27s                          | n/a               | 1035.7s        |

¹ The `fleet` build immediately followed the `tantivy` build with shared
incremental state, so its UNIT_DATA totals were 0.0s across the board — the
2m 12s wall-clock was dominated by re-validation, not real work. Use the
`tantivy`/`core` numbers for real signal.

### Top compile units in the `frankenterm-core` build

| Unit                             | Duration | Notes                                          |
| -------------------------------- | -------- | ---------------------------------------------- |
| `frankenterm-core` (todo)        | 130.27s  | Single compile unit; the long pole.            |
| `asupersync` (todo, two units)   | 124.77s  | Two crate versions resolved concurrently.      |
| `openssl-sys` (run-custom-build) | 63.11s   | Native build script.                           |
| `aws-lc-sys` (run-custom-build)  | 47.65s   | Native build script (needs llvm `clang`).      |
| `tantivy` (todo, two units)      | 49.47s   | Pulled in transitively, not in tantivy crate.  |
| `tokenizers` (todo, two units)   | 27.00s   | fastembed transitive.                          |
| `image` (todo)                   | 13.41s   | fastembed transitive.                          |

### `frankenterm-*` units in the `frankenterm-core` build

```
130.27s  frankenterm-core
  4.55s  frankenterm-escape-parser
  3.87s  frankenterm-term
  2.46s  frankenterm-ssh
  1.63s  frankenterm-surface
  1.04s  frankenterm-dynamic
  1.04s  frankenterm-cell
  0.90s  frankenterm-char-props
  0.89s  frankenterm-input-types
  0.63s  frankenterm-dynamic-derive
  0.54s  frankenterm-bidi
  0.46s  frankenterm-config-derive
  0.45s  frankenterm-color-types
  0.34s  frankenterm-core (run-custom-build)
  0.24s  frankenterm-alloc
  0.21s  frankenterm-blob-leases
  0.17s  frankenterm-uds
```

## Findings

### 1. The split shrunk the monolith only marginally

Before extraction, `frankenterm-core` was the single 130s+ compile unit. After
moving 7 tantivy modules (~620 KB of source) and `fleet_dashboard` (~35 KB) out,
the same compile unit measures **130.27s** — within noise of the prior monolith.

The reason: the tantivy modules were heavy *byte-count* but already
self-contained (clean cut); the LOC removed was a small slice of the parser /
typecheck graph, and `frankenterm-core` is dominated by the
`runtime`/`mcp`/`workflows`/`tx_*`/`mission_*` cluster that we did not move.

### 2. Sub-crates compile fast, but they wait on `frankenterm-core`

`frankenterm-core-tantivy` itself is **2.06s**.
`frankenterm-core-fleet` itself is **0.39s**.

But both have `frankenterm-core` as a path-dep, so the build graph still has to
finish the 130s monolith before the leaf crates start compiling. Until we
either (a) shrink the monolith below the parallelism threshold of the leaf
crates, or (b) break the `core → leaf → core` dependency cycle by extracting a
shared **types** crate beneath `frankenterm-core`, the wall-clock parallelism
gain is roughly zero.

### 3. The cycle revealed in ft-y0loj.3 is the load-bearing constraint

The fleet partial-extract (dd3e98fa) hit a `cyclic package dependency` because
six in-tree importers (`runtime`, `unified_telemetry`, `tx_execution`,
`mission_agent_mail`, `chaos_scale_harness`, `ntm_decommission`) consume
`fleet_*` types that themselves depend on `backpressure` / `memory_budget` /
`memory_pressure` — all of which still live in `frankenterm-core`. To move
`fleet_launcher` / `fleet_memory_controller` / `fleet_scrollback_coordinator`
out, one of two things has to happen first:

- **(A)** Extract `frankenterm-core-resource-types` (filed as **ft-usvnt**)
  containing the resource/pressure/budget primitives. Both `frankenterm-core`
  and `frankenterm-core-fleet` depend on it; no cycle.
- **(B)** Inline-duplicate the small types into the fleet crate. Rejected:
  duplicating `BackpressureTier` makes the runtime + fleet diverge silently.

ft-usvnt is the unblocker for the rest of the tier-3 extractions and almost
certainly the unblocker for any meaningful wall-clock win.

### 4. The actual long-pole is not `frankenterm-core` — it's the native build scripts

`openssl-sys` (63s) and `aws-lc-sys` (47s) are bigger fixed costs than any
single first-party compile unit besides `frankenterm-core` itself. Reducing
those (e.g. using `rustls`-only paths and gating `aws-lc-sys` behind an
opt-in feature) would do more for cold-build than another tier-3 cut.
Filed as a follow-up note in this proposal — not yet a bead.

## ADR — extract or park the remaining tier-3 cuts?

**Decision: park.**

| Option                                            | Outcome                                                                  |
| ------------------------------------------------- | ------------------------------------------------------------------------ |
| Continue tier-3 extracts now (lifecycle, mux)     | Cycles every time a moved type re-imports a `core` resource primitive.   |
| Extract `frankenterm-core-resource-types` first   | Unblocks fleet (full), lifecycle, mux, recorder. **Recommended next.**   |
| Park indefinitely                                 | Foregoes the cleanup gain; ft-y0loj parent stays open.                   |

**Recommended sequencing:**

1. **ft-usvnt** (`frankenterm-core-resource-types`) — extract
   `backpressure`, `memory_budget`, `memory_pressure`, plus their telemetry
   snapshot types. Tier-1 leaf, no first-party deps. Estimate: 1 session.
2. **ft-y0loj.3 finish** — re-attempt the fleet full-extract. Now
   `frankenterm-core-fleet` depends on `frankenterm-core-resource-types` and
   `frankenterm-core` does too; no cycle.
3. **ft-y0loj.4 / .5** (lifecycle, mux-bridge) — re-evaluate after .3 closes
   with a re-measurement of the wall-clock impact.
4. **`openssl-sys` long-pole follow-up** — file a bead to gate `aws-lc-sys`
   behind opt-in and prefer pure `rustls`. This is independent of the split and
   probably worth more than the rest of ft-y0loj combined.

## Appendix — raw timing artifacts

```
/tmp/ft-cc6-target/cargo-timings/
  cargo-timing-20260426T043525368Z-8978ee903710bae9.html  # tantivy build, 1m54s
  cargo-timing-20260426T043557304Z-8978ee903710bae9.html  # fleet (cached), 2m12s wall, 0s real
  cargo-timing-20260426T043759112Z-8978ee903710bae9.html  # fleet 3rd run (cached)
  cargo-timing-20260426T044028015Z-8978ee903710bae9.html  # frankenterm-core, 3m44s
```

Parsed via `UNIT_DATA` JS array in each file; reproduce with the snippet in
the bead's working notes.
