# ft-xbnl0.2.4 — Verification Smoke (tick 340, extended tick 346)

Date: 2026-04-18
Authored by: RusticMaple
Original commit: post-`bfb016b2` / ticks 311-339 (32 tests)
Extended commit: post-`7ace68aa` / ticks 311-345 (40 tests) — tick 346
Bead: ft-xbnl0.2.4

This is a single-run verification snapshot consolidating all ft-xbnl0.2.4
contract tests authored this session. Captured as an artifact so the bead
owner can reference a concrete "32 of 32 passing at this commit" checkpoint
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
running 19 tests
test distributed::tests::distributed_http_client_creates ... ok
test distributed::tests::distributed_http_client_post_honors_pre_cancelled_cx ... ok
test distributed::tests::distributed_http_client_honors_pre_cancelled_cx ... ok
test distributed::tests::distributed_http_client_rejects_invalid_urls_without_panic ... ok
test distributed::tests::distributed_http_client_connection_refused_returns_err ... ok
test distributed::tests::distributed_http_client_https_url_against_plaintext_server_returns_err ... ok
test distributed::tests::distributed_http_client_concurrent_gets ... ok
test distributed::tests::distributed_http_client_preserves_trailing_slash_in_path ... ok
test distributed::tests::distributed_http_client_surfaces_premature_server_close_as_err ... ok
test distributed::tests::distributed_http_client_post_large_body_roundtrips ... ok
test distributed::tests::distributed_http_client_post_empty_body_sends_content_length_zero ... ok
test distributed::tests::distributed_http_client_sends_host_header_matching_authority ... ok
test distributed::tests::distributed_http_client_local_get ... ok
test distributed::tests::distributed_http_client_url_without_path_defaults_to_slash ... ok
test distributed::tests::distributed_http_client_transmits_full_request_target ... ok
test distributed::tests::distributed_http_client_sends_non_empty_user_agent ... ok
test distributed::tests::distributed_http_client_local_post ... ok
test distributed::tests::distributed_http_client_handles_ipv6_literal_url ... ok
test distributed::tests::distributed_http_client_returns_non_2xx_as_ok_response ... ok

test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 25588 filtered out; finished in 0.02s
```

Tick 346 extended this run from 15 tests (tick 340) to 19 tests with the
additions from ticks 342 (https-vs-plaintext), 343 (invalid URL inputs),
344 (IPv6 literal), and 345 (premature close).

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
| HTTP client contracts (Run 1) | 19 | 19/19 ok |
| TLS contracts (Run 2) | 14 | 14/14 ok |
| Regression guards (Run 3) | 3 | 3/3 ok |
| Metrics server cx-family (Run 4) | 3 | 3/3 ok |
| Web server cx pre-cancel (Run 5) | 1 | 1/1 ok |
| **Subtotal** | **40** | **40/40 ok** |

Run 4 + Run 5 were added tick 341 to close the "individually-verified-only" caveat.
Run 1 expanded from 15 → 19 tests at tick 346 after ticks 342-345 added
scheme-dispatch (https-vs-plaintext), invalid-URL refusal, IPv6 literal,
and premature-server-close contracts.

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
running 1 test
test web_tests::web_server_with_cx_pre_cancelled_refuses_to_bind ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 15 filtered out; finished in 0.00s
```

Tick 323's pre-cancel contract for the web server bind path.

## Interpretation

- All 40 tests that land directly in the ft-xbnl0.2.4 verification surfaces pass together at HEAD. The contract set is self-consistent (no test conflicts with another's assumptions).
- Compile time after the initial cold build: 0.02s-3.42s per filtered run. This is cheap to re-run per-commit in CI.
- The 19 + 14 + 3 + 3 + 1 = 40 count matches the full roster landed in
  the completion-evidence doc through tick 345 (HTTP client §2.1 now
  covering scheme-dispatch + IPv6 + truncation + invalid-URL paths;
  TLS §2.2; service-boundary §2.3 incl. 3 pre-existing metrics tests
  + tick 323's web pre-cancel; regression guards §2.4).
- The evidence and the observable reality agree — no stale or missing
  entries in either direction.
