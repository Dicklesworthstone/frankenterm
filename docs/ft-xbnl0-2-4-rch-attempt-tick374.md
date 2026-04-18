# ft-xbnl0.2.4 — First real `rch exec` remote verification (tick 374)

Date: 2026-04-18
Tick: 374
Worker: vmi1149989 (212.90.121.76)
Remote run wall-time: **116.0s** (incl. 6.2s project sync + remote compile + test exec + 2.2s artifact return)

This is the first `rch exec` remote verification captured during this
session. Up to tick 373, rch workers had been intermittently
unreachable (hence the fork-bypass-only pattern documented in
[ft-xbnl0-2-4-completion-evidence.md §3](ft-xbnl0-2-4-completion-evidence.md#3-local-verification-recipe-fork-bypass-pattern)).
Tick 373's probe showed 6 workers green, so tick 374 kicked a real
remote exec.

## Command

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-2-4-tick374-guards \
    cargo test -p frankenterm-core \
    --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls
```

This matches the §4b individual-command form in the completion-evidence
doc (one of the narrower runs the closer can use to isolate a failure
group).

## Result

```
running 3 tests
test ft_xbnl0_2_4_asupersync_workspace_dep_present ... ok
test ft_xbnl0_2_4_no_tokio_net_deps_in_workspace_manifests ... ok
test ft_xbnl0_2_4_no_direct_tokio_tcp_tls_http_imports ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.01s

  INFO rch::hook: Remote command finished: exit=0 in 3257ms
  INFO rch::transfer: Artifacts retrieved in 2242ms (2545 files, 324 bytes)
  [RCH] remote vmi1149989 (116.0s)
```

**3/3 regression guards PASS on vmi1149989 remotely.**

## Breakdown

- Worker selection: `Selected worker: vmi1149989 at ubuntu@212.90.121.76 (0 slots, speed 50.0)`
- Project sync: `55 roots`, 4.4s for main tree + 1.8s for each sub-tree
- Remote compilation + test execution: `exit=0 in 3257ms`
- Artifact return: 2545 files (includes target/rch-ft-xbnl0-2-4-tick374-guards/ with build deps), 2.2s
- Total wall: 116.0s

## What this demonstrates

- The bead-scoped regression guards (ticks 311-317) are reproducible on
  a remote Contabo VPS, not just locally. The check script results are
  not a consequence of the local machine's particular state.
- The § 4b individual `rch exec` command form works. Closer can
  confidently use it for the per-group runs during closure.
- `target/rch-ft-xbnl0-2-4-<purpose>/` target dir pattern works —
  isolates this run's build products from other concurrent rch runs.

## Captured artifact

`/tmp/ft-xbnl0.2.4-rch-artifacts-tick374/rch-guards.log` (29 KB) — full
rch stdout/stderr including worker INFO logs, sync stats, cargo output,
test result lines, artifact return stats.

This is the Level-C artifact bundle entry for the regression-guard
run. Re-running the other 4 groups (HTTP client, TLS, metrics, web)
via the same pattern would give the complete Level-C bundle; deferred
to the closer since those are larger and would take longer wall time.

### Wall-time note for larger groups (tick 375 attempt)

Kicking off the HTTP-client group via rch
(`cargo test --features distributed,asupersync-runtime --lib distributed_http_client_`)
with a fresh target dir hit the 20-minute wall-time limit **mid-compile**
on vmi1149989 — it was still linking frankenterm-alloc, fastmcp-*,
mux, frankensearch as of the 1200s timeout. A cold remote build
pulling in the full `distributed` + `asupersync-runtime` feature graph
is ~30 min on a Contabo VPS; warm builds (reusing a target dir from a
prior run) are seconds.

Closer recommendation: do the first HTTP-client rch run with a
higher-ceiling timeout (e.g. 45 min) OR pre-warm by running the
guards group first into the same target dir so dependency compilation
is shared. Partial log at `/tmp/ft-xbnl0.2.4-rch-artifacts-tick374/rch-http.log`
(56 KB, mid-compile — no test result yet).

### HTTP-client rch run via warm-cache strategy (tick 392)

Tick 375 hit the 20-min wall-time mid-compile when running the
HTTP-client group with a fresh target dir. Tick 392 retried using
the SAME target dir as tick 391's successful guards run
(`target/rch-ft-xbnl0-2-4-tick391-guards`) so the base workspace
deps would be warm-cached on the remote worker.

**Result**: **29/29 HTTP client tests PASS** on vmi1149989.

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-2-4-tick391-guards \
    cargo test -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib distributed_http_client_
```

```
running 29 tests
test result: ok. 29 passed; 0 failed; finished in 0.12s
[RCH] remote vmi1149989 (589.6s)
```

Total wall: 589.6s (~10 min). Test exec itself: 0.12s. The remaining
~10 min was dominated by feature-conditional recompile (the guards
run uses default features; the HTTP run adds `distributed` +
`asupersync-runtime`, which pulls in rustls, webpki, H1 HTTP client
code, etc. that weren't in the guards build).

**Warm-cache strategy confirmed**: reusing an existing target dir
shaves ~10+ min off a cold HTTP/TLS rch run. Closer recommendation:
always run the guards group first (fast, ~2 min cold), THEN the
HTTP / TLS / metrics / web groups can reuse the target dir.

### Level-C evidence coverage rollup — COMPLETE at tick 393

All 6 test groups verified remotely on `vmi1149989` using the warm
target-dir strategy (tick 393).

| Group | Tests | Remote result | Wall | Tick |
|-------|-------|---------------|------|------|
| Regression guards | 3 | 3/3 PASS | 116s (cold) / 484s (re-confirm, fresh dir) | 374 + 391 |
| HTTP client contracts | 29 | **29/29 PASS** | 589s (warm with guards) | 392 |
| TLS tests | 45 | **45/45 PASS** | **116s (fully warm)** | 393 |
| Metrics server cx-family | 3 | **3/3 PASS** | 114s | 393 |
| Web server cx pre-cancel | 1 | **1/1 PASS** | 122s | 393 |
| runtime_compat primitive | 2 | **2/2 PASS** | 113s | 393 |
| **Total** | **83** | **83/83 PASS remotely** | | |

**All 83 ft-xbnl0.2.4 tests have remote Level-C evidence.** Once the
target dir is warm (first guards run: ~2 min cold, then HTTP: ~10
min adding feature graph), every subsequent group completes in
~2 min wall since the feature-conditional deps are cached.

### Command recipe for full Level-C capture

To reproduce the complete remote evidence bundle, run these in order
with the SAME target dir:

```bash
# 1. Cold run (guards, no features) — establishes warm target dir.
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls

# 2. HTTP client (adds distributed + asupersync-runtime features).
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib distributed_http_client_

# 3-5. TLS + metrics + web (all reuse the warm target).
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --features distributed,asupersync-runtime \
    --lib tls_

rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --features distributed,asupersync-runtime,web \
    --lib metrics_server_start_with_cx_

rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --features web,asupersync-runtime \
    --test web web_server_with_cx_

# 6. Primitive budget tests.
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0.2.4-full \
    cargo test -p frankenterm-core \
    --features asupersync-runtime \
    --lib _with_cx_observes_budget_deadline
```

Total wall time: ~20 min for the first two commands (cold+HTTP), then
~2 min each for the remaining four. ~28 min total.

### Re-confirmation at HEAD (tick 391, post-ft-l9mxa fix)

Re-ran the guards rch test after the tick-387 ft-l9mxa fix landed
to confirm the remote-build path still works at HEAD:

```bash
rch exec -- env CARGO_TARGET_DIR=target/rch-ft-xbnl0-2-4-tick391-guards \
    cargo test -p frankenterm-core \
    --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls
```

Result: **3/3 guards PASS** on vmi1149989 (same worker as tick 374).

```
running 3 tests
test ft_xbnl0_2_4_asupersync_workspace_dep_present ... ok
test ft_xbnl0_2_4_no_tokio_net_deps_in_workspace_manifests ... ok
test ft_xbnl0_2_4_no_direct_tokio_tcp_tls_http_imports ... ok

test result: ok. 3 passed; 0 failed; finished in 1.26s
[RCH] remote vmi1149989 (484.3s)
```

Total wall: 484.3s — longer than tick 374's 116s because the target
dir was fresh (tick 391 uses `-tick391-` vs tick 374's `-tick374-`)
and the workspace had grown with all the new tests/docs between
the two runs (thus more to sync). Test-exec time itself was 1.26s.

**Evidence**: the ft-xbnl0.2.4 remote verification path continues
to work at HEAD. All code changes between tick 374 and tick 391
(including ticks 375-390 test additions and the ft-l9mxa fix) are
remote-build-compatible.

Log at `/tmp/ft-xbnl0.2.4-rch-artifacts-tick374/rch-guards-tick391.log`
(50 KB).

## Implication for §6 closure checklist

- The `rch exec -- ./scripts/check_ft_xbnl0_2_4.sh` form (§4a one-shot)
  does NOT go remote — rch's hook only intercepts cargo compilation
  commands, so the shell script falls through to local exec (confirmed
  by the tick-374 earlier attempt which produced local output).
- The closer should use the §4b individual-command form for actual
  remote Level-C captures. §4a is still useful for local smoke (and
  runs the same 5 filtered cargo test commands under the hood).

Recommend minor doc update in the completion-evidence §4a to note this,
so a closer doesn't assume `rch exec -- scripts/...` sends the whole
script remote.
