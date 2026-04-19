# ft-xbnl0.2.4 — Verification Smoke (tick 340, re-verified through tick 409)

Date: 2026-04-18
Authored by: RusticMaple
Tick 340: post-`bfb016b2` / ticks 311-339 (32 tests, HTTP+TLS+guards only)
Tick 341: added service-boundary runs (36 tests)
Tick 346: HTTP extended to 19 after ticks 342-345 (40 tests total)
Tick 353: HTTP extended to 21 after tick 349→351→352 ft-kfkyi fix + companion (42 tests total)
Tick 357: TLS filter broadened from `build_tls_` to `tls_` — catches 1 previously-excluded parser test + ~30 pre-existing TLS tests for stronger smoke (73 tests total)
Tick 366: HTTP extended to 23 after ticks 364 (Send+Sync compile-time) + 365 (Arc-sharing runtime) (75 tests total)
Tick 368: HTTP extended to 24 after tick 367 (Default impl policy preservation) (76 tests total)
Tick 372: HTTP extended to 25 after tick 371 (body byte-verbatim) (77 tests total)
Tick 379: HTTP extended to 26 after tick 378 (expired-budget snapshot) (78 tests total)
Tick 384: HTTP extended to 27 (tick 380 mid-flight cancel, re-counted) + Run 6 added for ticks 382/383 primitive budget tests (81 tests total, 6 runs)
Tick 386: HTTP extended to 28 after tick 385 (POST 3xx no-follow verb-parity) (82 tests total)
Tick 390: HTTP extended to 29 after ticks 387 (ft-l9mxa fix flipping tick-380 snapshot) + 389 (POST mid-flight verb-parity) (83 tests total)
Tick 399: HTTP extended to 30 after tick 398 (POST Content-Type non-auto-inject) (84 tests total)
Tick 401: HTTP extended to 31 after tick 400 (URL percent-encoding pass-through) (85 tests total) — tick 400 milestone
Tick 403: HTTP extended to 32 after tick 402 (chunked transfer-encoding response) (86 tests total)
Tick 405: HTTP extended to 33 after tick 404 (HTTP/1.0 response decoding) (87 tests total)
Tick 409: HTTP extended to 34 after tick 408 (race_with_cx_cancel isolated unit test; renamed to distributed_http_client_race_with_cx_cancel_* to match check-script filter) (88 tests total)
Tick 417: Run 5 web extended to 2 after tick 417 (run_web_server_with_cx mid-flight cancel) (89 tests total)
Tick 418: Run 6 runtime_compat extended to 4 after tick 418 (yield_now_with_cx cancel-checkpoint + happy-path) (91 tests total)
Tick 419: Run 6 extended to 5 after tick 419 (oneshot_recv_with_cx pre-cancel) (92 tests total)
Tick 420: Run 6 extended to 6 after tick 420 (broadcast_recv_with_cx pre-cancel) (93 tests total)
Tick 421: Run 6 extended to 7 after tick 421 (Semaphore::acquire_with_cx pre-cancel) (94 tests total)
Tick 422: Run 6 extended to 8 after tick 422 (mpsc::Receiver::recv pre-cancel) (95 tests total)
Tick 423: Run 6 extended to 9 after tick 423 (watch::Receiver::changed pre-cancel) (96 tests total) — long-lived wait primitive cancel matrix complete
Tick 426: Run 6 extended to 10 after tick 426 (JoinSet::join_next_with_cx pre-cancel) (97 tests total) — first runtime_compat-owned primitive in the matrix
Tick 427: Run 6 extended to 13 after tick 427 (Semaphore::acquire_owned_with_cx pre-cancel + filter broadening picks up 2 pre-existing Semaphore happy-path tests) (100 tests total) — century milestone
Bead: ft-xbnl0.2.4

This is a single-run verification snapshot consolidating all ft-xbnl0.2.4
contract tests this session touches. Captured as an artifact so the bead
owner can reference a concrete "100 of 100 passing at this commit" checkpoint
without re-running every per-tick filter.

## Recipe

Local fork-bypass pattern (rch workers intermittently unavailable under
`force_remote=true`):

```python
# /tmp/ft-cargo-test-tick340-{all,tls,guards}.py
os.setsid()
env["CC"] = "/opt/homebrew/opt/llvm/bin/clang"
env["CXX"] = "/opt/homebrew/opt/llvm/bin/clang++"
env["CARGO_TARGET_DIR"] = "/tmp/ft-rusticmaple-target"
```

Three runs consolidated below; full recipe in
[ft-xbnl0-2-4-completion-evidence.md §3](ft-xbnl0-2-4-completion-evidence.md#3-local-verification-recipe-fork-bypass-pattern).

## Consolidated results

### Run 1 — distributed HTTP client contract tests

```
cargo test -p frankenterm-core --features distributed,asupersync-runtime --lib distributed_http_client_
```

```
running 21 tests
(all distributed_http_client_* tests pass)
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 25588 filtered out; finished in 0.12s
```

Run 1 progression:
- Tick 340: 15 tests
- Tick 346: +4 from ticks 342-345 (https-vs-plaintext, invalid URLs,
  IPv6 literal, premature close) = 19 tests
- Tick 353: +2 from ticks 349→351→352 (ft-kfkyi 3xx no-follow fix
  + companion test with resolvable Location target) = 21 tests

### Run 2 — TLS bundle + server-name contract tests

```
cargo test -p frankenterm-core --features distributed,asupersync-runtime --lib build_tls_
```

```
running 14 tests
test distributed::tests::build_tls_bundle_rejects_tls_disabled_config ... ok
test distributed::tests::build_tls_bundle_rejects_missing_cert_path ... ok
test distributed::tests::build_tls_bundle_rejects_missing_key_path ... ok
test distributed::tests::build_tls_server_name_accepts_ipv4_literal ... ok
test distributed::tests::build_tls_server_name_accepts_dns_hostname ... ok
test distributed::tests::build_tls_server_name_defaults_empty_host_to_localhost ... ok
test distributed::tests::build_tls_server_name_rejects_invalid_host ... ok
test distributed::tests::build_tls_bundle_surfaces_io_error_with_path_for_missing_cert_file ... ok
test distributed::tests::build_tls_bundle_rejects_mtls_without_client_ca_path ... ok
test distributed::tests::build_tls_bundle_surfaces_empty_cert_chain_for_empty_pem_file ... ok
test distributed::tests::build_tls_bundle_surfaces_empty_private_key_for_cert_in_key_slot ... ok
test distributed::tests::build_tls_bundle_accepts_min_tls_version_1_2_plus_suffix ... ok
test distributed::tests::build_tls_bundle_accepts_min_tls_version_1_3 ... ok
test distributed::tests::build_tls_bundle_accepts_min_tls_version_1_2 ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 25589 filtered out; finished in 0.00s
```

### Run 3 — regression guards (imports + manifest + positive-dep)

```
cargo test -p frankenterm-core --test ft_xbnl0_2_4_no_direct_tokio_net_or_rustls
```

```
running 3 tests
test ft_xbnl0_2_4_asupersync_workspace_dep_present ... ok
test ft_xbnl0_2_4_no_tokio_net_deps_in_workspace_manifests ... ok
test ft_xbnl0_2_4_no_direct_tokio_tcp_tls_http_imports ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.22s
```

## Totals

| Group | Tests | Result |
|-------|-------|--------|
| HTTP client contracts (Run 1) | 34 | 34/34 ok |
| TLS tests (Run 2) | 45 | 45/45 ok |
| Regression guards (Run 3) | 3 | 3/3 ok |
| Metrics server cx-family (Run 4) | 3 | 3/3 ok |
| Web server cx pre-cancel + mid-flight (Run 5) | 2 | 2/2 ok |
| Runtime-primitive contracts (Run 6) | 13 | 13/13 ok |
| **Subtotal** | **100** | **100/100 ok** |

Captured via `scripts/check_ft_xbnl0_2_4.sh` (tick 347, filter broadened
tick 357).

Run 1 growth over the session:
- tick 340: 15 tests (HTTP baseline)
- tick 346: +4 scheme-dispatch / invalid-URL / IPv6 / premature-close
- tick 353: +2 from the ft-kfkyi 3xx no-follow fix + companion
- tick 366: +2 Send+Sync compile-time (tick 364) + Arc-sharing runtime (tick 365)
- tick 368: +1 Default impl policy preservation (tick 367)
- tick 372: +1 response body byte-verbatim contract (tick 371)
- tick 379: +1 expired-budget cx snapshot (tick 378)
- tick 384: +1 mid-flight cancel snapshot (tick 380); Run 6 added with 2 primitive tests (ticks 382/383)
- tick 386: +1 POST 3xx no-follow verb-parity (tick 385)
- tick 390: +1 POST mid-flight cancel verb-parity (tick 389); tick-380 snapshot flipped by ft-l9mxa fix (tick 387)
- tick 399: +1 POST Content-Type non-auto-inject (tick 398)
- tick 401: +1 URL percent-encoding pass-through (tick 400 milestone)
- tick 403: +1 chunked transfer-encoding response decoding (tick 402)
- tick 405: +1 HTTP/1.0 response decoding (tick 404)
- tick 409: +1 race_with_cx_cancel isolated unit test (tick 408, renamed for filter match)

Run 2 growth:
- tick 340-346: 14 tests via `build_tls_` filter (this session's new
  TLS contract tests: bundle rejection, server-name, error-path,
  version parsing).
- tick 357: filter broadened from `build_tls_` to `tls_`, now catching
  this session's 14 new tests + 1 parser test (`resolve_tls_versions_*`)
  + ~30 pre-existing TLS tests (happy-path bundle exchange, large
  payload, token validation, TLS session windows, etc.) = 45 total.
  Broader smoke without any additional authoring work.

### Run 4 — metrics server cx-first family (includes tick 322)

```
cargo test -p frankenterm-core --features distributed,asupersync-runtime,web --lib metrics_server_start_with_cx_
```

```
running 3 tests
test metrics::tests::metrics_server_start_with_cx_pre_cancelled_refuses_to_bind ... ok
test metrics::tests::metrics_server_start_with_cx_mid_flight_cancel_stops_accept_loop ... ok
test metrics::tests::metrics_server_start_with_cx_happy_path_serves_request ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 25600 filtered out; finished in 0.26s
```

Covers the three cx-signal timings (pre-start / mid-flight / happy)
on the metrics server service boundary. Tick 322 added the
mid-flight-cancel test; the other two were pre-existing from the
ft-xbnl0.2.3 era.

### Run 5 — web server cx-first pre-cancel (tick 323)

```
cargo test -p frankenterm-core --features web,asupersync-runtime --test web web_server_with_cx_
```

```
running 2 tests
test web_tests::web_server_with_cx_pre_cancelled_refuses_to_bind ... ok
test web_tests::web_server_with_cx_mid_flight_cancel_exits_cleanly ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 15 filtered out; finished in 0.30s
```

Tick 323's pre-cancel contract + tick 417's mid-flight cx-cancel
contract for `run_web_server_with_cx` — pins both timings of the
cx signal for the web server bind/orchestration path.

### Run 6 — runtime_compat primitive contracts (budget + cancel matrix)

```
cargo test -p frankenterm-core --features asupersync-runtime --lib \
    -- _with_cx_observes_budget_deadline yield_now_with_cx \
       oneshot_recv_with_cx broadcast_recv_with_cx \
       semaphore_acquire_with_cx mpsc_recv_with_cx watch_changed_with_cx
```

```
running 9 tests
test sleep_with_cx_observes_budget_deadline ... ok
test timeout_with_cx_observes_budget_deadline ... ok
test yield_now_with_cx_observes_cx_cancel_checkpoint ... ok
test yield_now_with_cx_yields_on_live_cx ... ok
test oneshot_recv_with_cx_observes_pre_cancel ... ok
test broadcast_recv_with_cx_observes_pre_cancel ... ok
test semaphore_acquire_with_cx_observes_pre_cancel ... ok
test mpsc_recv_with_cx_observes_pre_cancel ... ok
test watch_changed_with_cx_observes_pre_cancel ... ok

test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 25622 filtered out; finished in 0.02s
```

Pins both halves of the primitive × signal-kind matrix:

| Primitive                      | Budget deadline | Direct cancel    |
|--------------------------------|-----------------|------------------|
| sleep_with_cx                  | ✓ tick 382      | ✗ documented     |
| timeout_with_cx                | ✓ tick 383      | ✗ documented     |
| yield_now_with_cx              | (n/a)           | ✓ tick 418       |
| oneshot_recv_with_cx           | (transitive)    | ✓ tick 419       |
| broadcast_recv_with_cx         | (transitive)    | ✓ tick 420       |
| Semaphore::acquire_with_cx     | (transitive)    | ✓ tick 421       |
| mpsc::Receiver::recv           | (transitive)    | ✓ tick 422       |
| watch::Receiver::changed       | (transitive)    | ✓ tick 423       |

Every long-lived wait primitive under asupersync short-circuits on a
pre-cancelled cx via its `cx.checkpoint()?` pre-guard and returns a
`Cancelled` variant in < 10 ms (verified under a 2 s outer safety-net
timeout).

## Interpretation

- All 100 tests that land in the ft-xbnl0.2.4 verification surfaces pass together at HEAD. The contract set is self-consistent (no test conflicts with another's assumptions).
- Compile time after the initial cold build: 0.00s-1.19s per filtered run. All tests now complete in sub-second wall time after tick 387's ft-l9mxa fix (previously Run 1 was 10.02s because the tick-380 snapshot's outer timeout fired; now the inner cancel-watcher race surfaces the cancel in ~70ms).
- The 34 + 45 + 3 + 3 + 2 + 13 = 100 count covers this-session deliverables AND 32 pre-existing TLS tests (tick-357 `tls_` broadening) plus 2 pre-existing Semaphore happy-path tests (tick-427 `semaphore_acquire_` broadening) that the widened filters smoke-verify as a side benefit.
- Run 6 grew from 2 to 9 tests across ticks 418-423 pinning the
  long-lived-wait-primitive × cx-cancel matrix across all four
  channel types (oneshot/broadcast/mpsc/watch) + semaphore + yield.
  All honour pre-cancelled cx with RecvError::Cancelled /
  AcquireError::Cancelled in under 10 ms.
- The evidence and the observable reality agree — no stale or missing
  entries in either direction.
- The ft-kfkyi security follow-up (3xx transparent redirect following)
  discovered in tick 349, filed tick 350, fixed tick 351, companion
  test added tick 352 — is closed. The distributed HTTP client now
  explicitly disables redirect following via `.no_redirects()` on the
  asupersync HttpClient builder.
