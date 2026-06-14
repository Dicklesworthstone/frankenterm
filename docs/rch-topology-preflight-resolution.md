# RCH installed topology-alias preflight fix — resolution evidence (ft-4tp7g.3)

## Summary

The installed-RCH topology-alias preflight blocker tracked by **ft-4tp7g.3**
is **resolved**. The installed FrankenTerm RCH daemon/CLI has been updated past
v1.0.26 (the version that carried the old `ln: Already exists` topology
preflight failure) to **1.0.41**, and a material remote-required smoke now
reaches **transfer + remote Cargo execution** without the topology-preflight
failure.

This closes acceptance item 1 (the fix is available through the supported
installed RCH path — `rch --version` now reports a build that includes the
`remote_compilation_helper` topology-alias preflight fix) and item 2 (a
material remote-required smoke reaches transfer/remote execution without the
old `ln: Already exists` failure).

## Evidence (2026-06-14)

| Field | Value |
|-------|-------|
| Installed RCH identity | `rch 1.0.41 (commit e2ed51271047)` — previously 1.0.26 |
| Worker selected | `vmi1149989` at `root@212.90.121.76` |
| RCH job | `j-29884604911452506` |
| Command | `RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-4tp7g3-cc4-target cargo test -p frankenterm-core-connector-types --lib -- --nocapture` |
| Target crate | `frankenterm-core-connector-types` (narrow leaf crate; intentionally **not** `frankenterm-core --lib`, which wedges) |
| Topology preflight | **PASSED** — no `ln: Already exists`; worker selection proceeded directly to the compilation pipeline |
| Transfer | **reached** — `Sync complete: 39829 files, 1862643 bytes` |
| Remote Cargo | **reached** — `Executing command remotely: env cargo test -p frankenterm-core-connector-types --lib`; CARGO_TARGET_DIR rewritten to the worker-scoped path |
| rustc / test verdict | **not reached in this run** — stalled on `Updating git repository asupersync` (the cargo built-in git-fetch). This is a **separate, known issue**, not topology — fix by adding `CARGO_NET_GIT_FETCH_WITH_CLI=true` to the inner env (see the `rch-git-fetch-cli-keeps-proof-remote` note). |

## Contrast with the prior blocked state

The 2026-05-16 → 2026-05-24 ft-4tp7g.3 comments recorded installed RCH **1.0.26**
selecting a worker and then failing **before** Cargo with remote topology
preflight stderr ending in `ln: Already exists`, refusing local fallback. The
source fix existed in the `remote_compilation_helper` repo
(`b052eb5` / `7a03b81` / `2f947c5` / `1cc375f`) but was unreachable through
`rch update` (stable/nightly both pinned to 1.0.26).

As of 2026-06-14 the installed build is **1.0.41**, the worker reaches the
compilation pipeline and transfer without the topology failure, so the source
fix is now live on the installed path.

## Operator follow-up (outside agent scope)

Agents must not deploy/restart/update RCH services. The installed update to
1.0.41 was performed through the supported operator path outside FrankenTerm
agent sessions. To take a narrow remote-required proof all the way to a green
test verdict, add `CARGO_NET_GIT_FETCH_WITH_CLI=true` to the inner env so the
`asupersync` git-fetch completes (a distinct concern from topology preflight).

With topology preflight resolved and RCH admitting workers again, the
deferred-proof backlog accumulated during the outage (source-landed beads
blocked solely on RCH) can now be re-proven via narrow per-crate `cargo test`
runs.
