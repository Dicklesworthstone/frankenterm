# storage.rs block_on audit (ft-3tvvt)

## Summary

`crates/frankenterm-core/src/storage.rs` currently contains 9 `block_on` call
sites. Only one is production library code. The other 8 live in the
`#[cfg(test)]` tail that cc_1's `ft-dn2tu` proposal already identified as the
first extraction target for the storage split.

The production issue is real: the writer thread creates a private
current-thread runtime so synchronous storage writer code can wait on an async
`mpsc::Receiver<WriteCommand>`. That makes a library module own runtime
progress instead of borrowing runtime progress from the caller or from a
dedicated bridge abstraction.

This proposal intentionally does not rewrite `storage.rs` inline. The
production call site is coupled to the writer thread shape, shutdown semantics,
and channel type. The test call sites are best removed with the `ft-dn2tu.1`
test extraction rather than churned in place inside a 34K-line file.

## Reference

This audit builds on `docs/proposals/ft-dn2tu-storage-split.md`, especially:

- Phase 1: extract the 17 `#[cfg(test)]` modules from `storage.rs`.
- Phase 6: defer the high-risk `StorageHandle`/writer split until the lower
  risk section moves make the call graph smaller.

## Call Site Matrix

| Site | Scope | Why it exists today | Minimal async refactor | Follow-on |
| --- | --- | --- | --- | --- |
| `storage.rs:12215` in `writer_loop` | Production | `writer_loop` is synchronous because it owns a `rusqlite::Connection` on a dedicated writer thread, but write commands arrive through `runtime_async::mpsc::Receiver`, whose blocking receive path is async and `Cx`-aware. The private runtime is used only to wait for the next command. | Introduce a small writer inbox abstraction with a synchronous `recv_blocking(&Cx)` API backed by the runtime channel internally, or move the writer loop into an async task that owns the connection and uses `spawn_blocking`/single-thread confinement for SQLite work. The smaller API-churn option is `StorageWriterInbox`: `StorageHandle` keeps async senders, while the writer thread receives through a bridge object that does not expose `block_on` in storage business logic. | `ft-ixgqo` |
| `storage.rs:23543` in top-level `run_async_test` | Test | Legacy storage async tests are plain `#[test]` functions that bootstrap a current-thread runtime locally. | Extract the helper during `ft-dn2tu.1` into `storage/tests/support.rs`, replace per-test runtime ownership with one shared `run_storage_async_test` helper that uses the project test runtime API and clears TLS consistently. | `ft-upvjr` |
| `storage.rs:27947` in `storage_handle_tests::run_async_test` | Test | Duplicate of the storage async test harness, with extra unwind guards for asupersync TLS cleanup. | Same shared helper as above; this module should call `storage::tests::support::run_storage_async_test(...)`. | `ft-rqsr3` |
| `storage.rs:29831` in `queue_depth_tests::run_async_test` | Test | Duplicate helper for queue-depth async assertions. | Same shared helper; keep queue-depth test bodies async and remove local runtime construction. | `ft-5uudv` |
| `storage.rs:30090` in `backpressure_integration_tests::run_async_test` | Test | Duplicate helper for backpressure tests that also use runtime channels directly. | Same shared helper; keep channel sends async and move only runtime ownership out of the module. | `ft-v4f6g` |
| `storage.rs:30460` in `prop_seq_monotonic_per_pane` | Test | Proptest bodies are synchronous, so the test creates a multi-thread runtime inside each generated case. | Add a proptest-specific helper that builds one runtime per proptest case through shared support and returns the async result value. Longer term, move these to an integration test with a runtime fixture. | `ft-iq339` |
| `storage.rs:30513` in `prop_fts_finds_inserted_text` | Test | Same proptest sync-to-async bridge as above. | Use the shared proptest helper so runtime creation/drop/TLS cleanup are centralized. | `ft-s39f3` |
| `storage.rs:30558` in `prop_fts_respects_pane_scope` | Test | Same proptest sync-to-async bridge as above. | Use the shared proptest helper so pane-scoped search properties do not manually own a runtime. | `ft-c7q63` |
| `storage.rs:33249` in `timeline_integration_tests::run_async_test` | Test | Duplicate helper for timeline async integration tests. | Same shared helper; extract with the test module in `ft-dn2tu.1`. | `ft-jcvq4` |

## Recommended Order

1. Fix the production writer inbox first (`ft-ixgqo`). This is the only
   runtime-ownership issue in non-test storage code.
2. Land `ft-dn2tu.1` or pair with it before test cleanup. The duplicated test
   `block_on` helpers are much easier to remove after the 11K-line test tail
   moves out of `storage.rs`.
3. Remove test helper duplication in one support module (`ft-upvjr`,
   `ft-rqsr3`, `ft-5uudv`, `ft-v4f6g`, `ft-iq339`, `ft-s39f3`,
   `ft-c7q63`, `ft-jcvq4`). These are not runtime correctness bugs in
   production, but they make the storage test surface look like library runtime
   debt.

## Non-Goals

- Do not replace `runtime_async` with direct `tokio`.
- Do not split `storage.rs` as part of this bead; that belongs to `ft-dn2tu`.
- Do not move the SQLite connection across async tasks without a dedicated
  confinement design. The current writer-thread ownership is intentional even
  though its receive bridge should be cleaner.

## Verification Commands

For this audit:

```bash
rg -n "block_on|RuntimeBuilder|current_thread|Handle::current" \
  crates/frankenterm-core/src/storage.rs \
  crates/frankenterm-core/src/storage -g'*.rs'
```

For follow-on implementation:

```bash
scripts/cargo-local.sh test -p frankenterm-core storage
```
