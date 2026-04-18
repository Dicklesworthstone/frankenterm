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
