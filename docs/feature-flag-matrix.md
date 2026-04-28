# Feature Flag Matrix

Bead: `ft-bzgxi`

This matrix is the citable feature-flag inventory for the FrankenTerm workspace.
It separates three different facts that were previously easy to conflate:

- Cargo declares 179 local feature keys across workspace packages.
- Rust source uses feature gates in concentrated surfaces such as distributed
  mode, vendored mux/runtime integration, MCP, terminal image/serde support,
  and Linux io_uring dispatch.
- CI cannot build the full N-by-M cross product. It builds the default surface,
  individual feature lanes, and the known-good combination lanes listed below.

## Source Commands

Regenerate the inventory with:

```bash
cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.source == null) | [.name, (.manifest_path|sub("^.*frankenterm/";"")), ((.features|keys)|join(","))] | @tsv'
```

Find code-gated features with:

```bash
rg -o '#\[cfg(?:_attr)?\([^\n]*feature\s*=\s*"[^"]+"|cfg!\(feature\s*=\s*"[^"]+"' crates frankenterm -g'*.rs' \
  | sed -E 's/.*feature\s*=\s*"([^"]+)".*/\1/' \
  | sort | uniq -c | sort -nr
```

Count feature keys with:

```bash
cargo metadata --no-deps --format-version 1 \
  | jq -r '[.packages[] | select(.source == null) | (.features|keys|length)] | add'
```

Current count: 179 feature keys.

## What CI Builds

| CI lane | Scope | Feature shape | Status |
| --- | --- | --- | --- |
| `lint` | Ubuntu + macOS | Workspace defaults, plus one optional set: `tui,ftui,rollout,mcp,browser,web,metrics,distributed` | Built/tested by CI |
| `test` | Ubuntu + macOS + Windows | Workspace defaults and doc tests | Built/tested by CI |
| `feature-matrix` | Ubuntu | Workspace default lane plus individual `tui`, `ftui`, `mcp`, `browser`, `web`, `metrics`, `distributed` lanes | Built/tested by CI |
| `runtime-matrix` | Ubuntu | `frankenterm-core` no-default and `asupersync-runtime` no-default modes | Built/tested by CI |
| `feature-vendored` | Ubuntu | Workspace `vendored` feature | Built/tested by CI |
| `feature-combination-matrix` | Ubuntu | Known-good multi-feature compile combinations from `scripts/check_feature_flag_matrix.sh` | Built/tested by CI after `ft-bzgxi` |

Theoretical combinations outside these rows are not CI-proven. They should be
promoted into `scripts/check_feature_flag_matrix.sh` before being cited as
supported.

## Known-Good Combination Gate

The new CI gate runs `bash scripts/check_feature_flag_matrix.sh` on Ubuntu.
Local verification can use the Cargo wrapper:

```bash
CARGO_BIN=scripts/cargo-local.sh bash scripts/check_feature_flag_matrix.sh
```

| Combo id | Command | Why this combination matters | CI status |
| --- | --- | --- | --- |
| `core-minimal` | `cargo check -p frankenterm-core --no-default-features --lib` | Cites the library floor without the standalone terminal platform default surface. | Checked by `feature-combination-matrix` |
| `core-runtime-mux` | `cargo check -p frankenterm-core --no-default-features --lib --features asupersync-runtime,vendored,native-wezterm` | Cites the structured runtime plus in-tree mux bridge without pulling unrelated UI/search surfaces. | Checked by `feature-combination-matrix` |
| `core-distributed-runtime` | `cargo check -p frankenterm-core --no-default-features --lib --features asupersync-runtime,distributed,metrics` | Cites the distributed TLS/control-plane path together with runtime and metrics gates. | Checked by `feature-combination-matrix` |
| `core-search-recorder` | `cargo check -p frankenterm-core --no-default-features --lib --features recorder-lexical,frankensearch,semantic-search` | Cites lexical, external search, and embedding-related compile surfaces together. | Checked by `feature-combination-matrix` |
| `cli-operator-surface` | `cargo check -p frankenterm --bin ft --no-default-features --features frankenterm,asupersync-runtime,mcp,web,browser,metrics,distributed,subprocess-bridge,sync` | Cites the operator-facing CLI without enabling all default features. | Checked by `feature-combination-matrix` |
| `mux-server-io-uring` | `cargo check -p frankenterm-mux-server --no-default-features --features io-uring` | Cites the Linux io_uring mux-server dispatch path. | Checked by `feature-combination-matrix` on Linux; skipped locally on non-Linux |
| `gui-headless-render` | `cargo check -p frankenterm-gui --no-default-features --features headless-render` | Cites the headless-render GUI compile surface without default GUI features. | Checked by `feature-combination-matrix` |

## Code-Gated Hot Spots

The `rg` scan shows the highest-density feature-gated areas:

| Feature area | Main locations | Notes |
| --- | --- | --- |
| `distributed` | `crates/frankenterm-core/src/distributed.rs`, `crates/frankenterm/src/main.rs` | Largest source-level feature surface; covered by existing individual CI and new runtime/distributed combo. |
| `vendored`, `native-wezterm`, `asupersync-runtime` | `crates/frankenterm-core/src/runtime.rs`, `crates/frankenterm-core/src/wezterm.rs`, `crates/frankenterm-core/src/vendored.rs` | Mux/runtime integration surface; covered by `core-runtime-mux` and `feature-vendored`. |
| `mcp`, `mcp-client`, `mcp-server`, `web`, `browser` | `crates/frankenterm-core/src/mcp*.rs`, `crates/frankenterm/src/main.rs` | Operator/API surfaces; covered individually and by `cli-operator-surface`. |
| `semantic-search`, `frankensearch`, `recorder-lexical` | `crates/frankenterm-core/src/search/`, recorder/search tests and benches | Covered by `core-search-recorder`. |
| `tui`, `ftui`, `tui-widgets`, `tui-dashboard` | `crates/frankenterm-core/src/tui/`, `crates/frankenterm-core/benches/` | Covered individually and by the existing optional clippy set. |
| `io-uring` | `crates/frankenterm-mux-server-impl/src/dispatch.rs` | Linux-only compile surface; covered by `mux-server-io-uring` in Ubuntu CI. |
| Vendored terminal features | `frankenterm/{termwiz,surface,cell,color-types,escape-parser,ssh,codec,config,mux,uds,async_ossl}` | Covered when built as dependencies of default/vendored lanes; not every vendored crate feature cross-product is CI-proven. |

## Cargo Feature Inventory

This table is from `cargo metadata --no-deps --format-version 1` on
2026-04-28.

| Package | Manifest | Declared features |
| --- | --- | --- |
| `frankenterm` | `Cargo.toml` | `asupersync-runtime`, `browser`, `default`, `distributed`, `frankenterm`, `ftui`, `jemalloc`, `mcp`, `metrics`, `native-wezterm`, `redis-session`, `rollout`, `semantic-search`, `subprocess-bridge`, `sync`, `tui`, `tui-dashboard`, `tui-widgets`, `vendored`, `web` |
| `frankenterm-alloc` | `crates/frankenterm-alloc/Cargo.toml` | `default`, `jemalloc` |
| `frankenterm-core` | `crates/frankenterm-core/Cargo.toml` | `__journal_types_placeholder`, `agent-detection`, `agent-mail`, `asupersync-runtime`, `browser`, `browser-automation`, `cass-export`, `default`, `disk-pressure`, `distributed`, `frankensearch`, `frankensqlite-recorder`, `frankenterm`, `frankenterm-deps`, `ftui`, `fuzz`, `mcp`, `mcp-client`, `mcp-server`, `metrics`, `native-wezterm`, `recorder-lexical`, `redis-session`, `rollout`, `semantic-search`, `session-resume`, `streaming`, `subprocess-bridge`, `sync`, `tui`, `tui-dashboard`, `tui-widgets`, `vc-export`, `vendored`, `vendored-wezterm`, `web` |
| `codec` | `codec/Cargo.toml` | `async-asupersync`, `async-smol`, `default`, `fuzzing` |
| `config` | `config/Cargo.toml` | `async-asupersync`, `async-smol`, `default`, `distro-defaults`, `lua`, `no-lua` |
| `frankenterm-bidi` | `bidi/Cargo.toml` | none |
| `frankenterm-dynamic` | `dynamic/Cargo.toml` | `std` |
| `frankenterm-dynamic-derive` | `dynamic/derive/Cargo.toml` | none |
| `frankenterm-config-derive` | `config/derive/Cargo.toml` | none |
| `frankenterm-input-types` | `input-types/Cargo.toml` | `default`, `serde`, `std` |
| `frankenterm-ssh` | `ssh/Cargo.toml` | `async-asupersync`, `async-smol`, `default`, `libssh-rs`, `ssh2`, `vendored-openssl`, `vendored-openssl-libssh-rs`, `vendored-openssl-ssh2` |
| `async_ossl` | `async_ossl/Cargo.toml` | `async-asupersync`, `async-io`, `default` |
| `filedescriptor` | `filedescriptor/Cargo.toml` | none |
| `frankenterm-uds` | `uds/Cargo.toml` | `async-asupersync`, `async-io`, `default` |
| `portable-pty` | `pty/Cargo.toml` | `default`, `serde`, `serde_support` |
| `termwiz` | `termwiz/Cargo.toml` | `cassowary`, `default`, `docs`, `fnv`, `frankenterm-blob-leases`, `image`, `pest`, `pest_derive`, `serde`, `sha2`, `tmux_cc`, `use_image`, `use_serde`, `widgets` |
| `frankenterm-blob-leases` | `blob-leases/Cargo.toml` | `default`, `serde`, `simple_tempdir` |
| `frankenterm-cell` | `cell/Cargo.toml` | `std`, `use_image`, `use_serde` |
| `frankenterm-char-props` | `char-props/Cargo.toml` | `default`, `serde`, `std`, `use_serde` |
| `frankenterm-color-types` | `color-types/Cargo.toml` | `csscolorparser`, `serde`, `std`, `use_serde` |
| `frankenterm-escape-parser` | `escape-parser/Cargo.toml` | `docs`, `frankenterm-blob-leases`, `image`, `kitty-shm`, `sha2`, `std`, `tmux_cc`, `use_image`, `use_serde` |
| `vtparse` | `vtparse/Cargo.toml` | `alloc`, `default`, `no_std`, `std` |
| `frankenterm-surface` | `surface/Cargo.toml` | `appdata`, `default`, `frankenterm-blob-leases`, `serde`, `std`, `use_image`, `use_serde` |
| `frankenterm-term` | `term/Cargo.toml` | `use_serde` |
| `luahelper` | `luahelper/Cargo.toml` | none |
| `promise` | `promise/Cargo.toml` | `async-asupersync`, `default` |
| `umask` | `umask/Cargo.toml` | none |
| `mux` | `mux/Cargo.toml` | `async-asupersync`, `async-smol`, `default`, `lua`, `no-lua` |
| `bintree` | `bintree/Cargo.toml` | none |
| `procinfo` | `procinfo/Cargo.toml` | `default`, `lua` |
| `rangeset` | `rangeset/Cargo.toml` | none |
| `base91` | `base91/Cargo.toml` | none |
| `frankenterm-core-cass-types` | `crates/frankenterm-core-cass-types/Cargo.toml` | none |
| `frankenterm-core-caut-types` | `crates/frankenterm-core-caut-types/Cargo.toml` | none |
| `frankenterm-core-config-types` | `crates/frankenterm-core-config-types/Cargo.toml` | none |
| `frankenterm-core-connector-types` | `crates/frankenterm-core-connector-types/Cargo.toml` | none |
| `frankenterm-core-error-types` | `crates/frankenterm-core-error-types/Cargo.toml` | none |
| `frankenterm-core-policy-types` | `crates/frankenterm-core-policy-types/Cargo.toml` | none |
| `frankenterm-core-replay-types` | `crates/frankenterm-core-replay-types/Cargo.toml` | none |
| `frankenterm-core-resource-types` | `crates/frankenterm-core-resource-types/Cargo.toml` | none |
| `frankenterm-core-telemetry-types` | `crates/frankenterm-core-telemetry-types/Cargo.toml` | none |
| `frankenterm-core-ars` | `crates/frankenterm-core-ars/Cargo.toml` | none |
| `frankenterm-core-fleet` | `crates/frankenterm-core-fleet/Cargo.toml` | none |
| `frankenterm-core-replay` | `crates/frankenterm-core-replay/Cargo.toml` | none |
| `frankenterm-core-tantivy` | `crates/frankenterm-core-tantivy/Cargo.toml` | none |
| `frankenterm-gui` | `crates/frankenterm-gui/Cargo.toml` | `default`, `dhat-ad-hoc`, `dhat-heap`, `headless-render`, `wayland` |
| `frankenterm-mux-server-impl` | `crates/frankenterm-mux-server-impl/Cargo.toml` | `default`, `io-uring` |
| `wezterm-client` | `client/Cargo.toml` | none |
| `ratelim` | `ratelim/Cargo.toml` | none |
| `frecency` | `frecency/Cargo.toml` | none |
| `lfucache` | `lfucache/Cargo.toml` | none |
| `mux-lua` | `lua-api-crates/mux-lua/Cargo.toml` | none |
| `termwiz-funcs` | `lua-api-crates/termwiz-funcs/Cargo.toml` | `async-asupersync`, `async-smol`, `default`, `lua` |
| `url-funcs` | `lua-api-crates/url-funcs/Cargo.toml` | none |
| `tabout` | `tabout/Cargo.toml` | none |
| `wezterm-font` | `font/Cargo.toml` | `vendor-jetbrains`, `vendor-nerd-font-symbols`, `vendor-noto-emoji`, `vendor-roboto` |
| `freetype` | `deps-freetype/Cargo.toml` | none |
| `harfbuzz` | `deps-harfbuzz/Cargo.toml` | none |
| `wezterm-toast-notification` | `toast-notification/Cargo.toml` | none |
| `wezterm-open-url` | `open-url/Cargo.toml` | none |
| `fontconfig` | `deps-fontconfig/Cargo.toml` | none |
| `frankenterm-gui-subcommands` | `gui-subcommands/Cargo.toml` | none |
| `window` | `window/Cargo.toml` | `smithay-client-toolkit`, `wayland`, `wayland-backend`, `wayland-client`, `wayland-egl`, `wayland-protocols`, `wayland-protocols-plasma` |
| `frankenterm-mux-server` | `crates/frankenterm-mux-server/Cargo.toml` | `default`, `io-uring`, `jemalloc` |
| `frankenterm-fuzz` | `fuzz/Cargo.toml` | `codec-fuzz-targets`, `core-fuzz-targets`, `default`, `mcp-fuzz-targets` |
| `frankenterm-scripting` | `scripting/Cargo.toml` | `default`, `lua`, `wasm` |
| `env-bootstrap` | `env-bootstrap/Cargo.toml` | none |
