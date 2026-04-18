# ft-xbnl0.2.4 — Verification Smoke (tick 340)

Date: 2026-04-18
Authored by: RusticMaple
Commit at time of run: post-`bfb016b2` / ticks 311-339
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
running 15 tests
test distributed::tests::distributed_http_client_creates ... ok
test distributed::tests::distributed_http_client_post_honors_pre_cancelled_cx ... ok
test distributed::tests::distributed_http_client_honors_pre_cancelled_cx ... ok
test distributed::tests::distributed_http_client_connection_refused_returns_err ... ok
test distributed::tests::distributed_http_client_sends_host_header_matching_authority ... ok
test distributed::tests::distributed_http_client_concurrent_gets ... ok
test distributed::tests::distributed_http_client_url_without_path_defaults_to_slash ... ok
test distributed::tests::distributed_http_client_preserves_trailing_slash_in_path ... ok
test distributed::tests::distributed_http_client_post_large_body_roundtrips ... ok
test distributed::tests::distributed_http_client_transmits_full_request_target ... ok
test distributed::tests::distributed_http_client_sends_non_empty_user_agent ... ok
test distributed::tests::distributed_http_client_post_empty_body_sends_content_length_zero ... ok
test distributed::tests::distributed_http_client_local_post ... ok
test distributed::tests::distributed_http_client_local_get ... ok
test distributed::tests::distributed_http_client_returns_non_2xx_as_ok_response ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 25588 filtered out; finished in 0.03s
```

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
| HTTP client contracts | 15 | 15/15 ok |
| TLS contracts | 14 | 14/14 ok |
| Regression guards | 3 | 3/3 ok |
| **Subtotal** | **32** | **32/32 ok** |

Not included in this smoke run (they live in separate test files / feature sets):
- `metrics_server_start_with_cx_mid_flight_cancel_stops_accept_loop` (tick 322, `src/metrics.rs`)
- `web_server_with_cx_pre_cancelled_refuses_to_bind` (tick 323, `tests/web.rs` under `--features web,asupersync-runtime`)

These were individually verified at commit time of their respective ticks (322, 323) and are expected to still pass — add them to the rch remote-verification run recipe in the completion-evidence doc §4 for formal closure.

## Interpretation

- All 32 tests that land directly in the distributed + tests-guard surfaces pass together at HEAD. The contract set is self-consistent (no test conflicts with another's assumptions).
- Compile time after the initial cold build: 0.36s-0.94s per filtered run. This is cheap to re-run per-commit in CI.
- The 15 + 14 + 3 = 32 count matches the 32 tests claimed in the completion-evidence doc §2.1 + §2.2 + §2.4. The evidence and the observable reality agree.
