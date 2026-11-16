# ft-xbnl0.5.7 Completion Evidence

## Scope

This slice improves operator-facing first-run, diagnostic, and recovery entrypoints in
`crates/frankenterm/src/main.rs` by adding shared operator guidance to:

- `ft status --health`
- `ft doctor`
- `ft session doctor`

The new guidance layer classifies bootstrap-required, recovery-required, blocked,
attention-required, watcher-stopped, and ready states, then emits concrete next-step
commands instead of generic one-line hints.

## Verification

Exact remote verification command that passed:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-jemanuel-target cargo check -p frankenterm
```

Result:

- `cargo check -p frankenterm` passed remotely on `vmi1149989`
- the new operator-guidance structs and command-surface wiring typechecked cleanly

## Notes

- Initial attempts that forced `CC=/opt/homebrew/opt/llvm/bin/clang` and `CC=clang`
  failed on the Linux worker before Rust compilation because those compiler paths were
  not present there.
- The successful validation used the worker's default toolchain with the required
  per-agent `CARGO_TARGET_DIR`.
