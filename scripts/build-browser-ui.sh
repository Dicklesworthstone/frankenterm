#!/usr/bin/env bash
# Build the browser fleet viewer package that `ft web --ui-dir` serves.
#
# The FrankenTerm web renderer (FrankenTermWeb, WebGPU) is built from its last
# source revision in the frankentui repository, verified by archive SHA-256,
# with the dependency lock vendored beside the viewer crate. The viewer runner
# is crates/frankenterm-browser-ui. The output's site/manifest.json names every
# served file with its SHA-256; ft web refuses a package whose files differ.
#
# Usage: scripts/build-browser-ui.sh <new-output-dir>
# Needs cargo, the wasm32-unknown-unknown target for the pinned toolchain, and
# the wasm-bindgen CLI at the version both locks pin. wasm-bindgen runs on this
# host, so build the wasm here too: RCH_CARGO_WRAPPER_BYPASS=1 when the rch
# cargo shim is installed.
set -euo pipefail

fail() { printf 'build-browser-ui: %s\n' "$*" >&2; exit 1; }

[[ $# -eq 1 ]] || fail 'usage: scripts/build-browser-ui.sh <new-output-dir>'
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$1
[[ ! -e "$out" && ! -L "$out" ]] || fail "output already exists: $out"
for tool in cargo rustc wasm-bindgen curl tar shasum awk sed; do
  command -v "$tool" >/dev/null || fail "missing tool: $tool"
done

renderer_revision=88b402b8be9c70a4405895d4172e449940cab2fe
renderer_archive_sha256=46e23154a20e22672465e406f5f79f5b9e800198c8f655e073d0e79ad0d05306
crate_dir="$repo/crates/frankenterm-browser-ui"
renderer_lock="$crate_dir/renderer.lock"

# One pinned toolchain builds both modules.
toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$repo/rust-toolchain.toml")
[[ "$toolchain" == nightly-????-??-?? ]] || fail "toolchain must be date-pinned, got: $toolchain"
export RUSTUP_TOOLCHAIN=$toolchain
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS

# The JS glue must come from the exact wasm-bindgen version each module links.
lock_version() {
  awk -v name="$1" '$0 == "name = \"" name "\"" { getline; gsub(/version = |"/, ""); print; exit }' "$2"
}
bindgen_version=$(wasm-bindgen --version | awk '{ print $2 }')
for lock in "$repo/Cargo.lock" "$renderer_lock"; do
  locked=$(lock_version wasm-bindgen "$lock")
  [[ "$locked" == "$bindgen_version" ]] ||
    fail "wasm-bindgen CLI $bindgen_version does not match $locked in $lock"
done

mkdir -p "$out"
out=$(cd "$out" && pwd)
mkdir "$out/renderer-source" "$out/site" "$out/site/pkg" "$out/site/fonts"
target=${CARGO_TARGET_DIR:-$out/target}

curl --fail --location --silent --show-error \
  --user-agent 'OpenAI File Downloader, XaiImageApiFetch/1.0' \
  --output "$out/renderer.tar.gz" \
  "https://codeload.github.com/Dicklesworthstone/frankentui/tar.gz/$renderer_revision"
actual=$(shasum -a 256 "$out/renderer.tar.gz" | awk '{ print $1 }')
[[ "$actual" == "$renderer_archive_sha256" ]] || fail "renderer archive checksum mismatch: $actual"
tar -xzf "$out/renderer.tar.gz" --strip-components=1 -C "$out/renderer-source"
[[ ! -e "$out/renderer-source/Cargo.lock" ]] || fail 'unexpected lock in the renderer archive'
cp "$renderer_lock" "$out/renderer-source/Cargo.lock"

(cd "$out/renderer-source" &&
  cargo build --locked --release --lib --target wasm32-unknown-unknown \
    --target-dir "$target/renderer" -p frankenterm-web)
wasm-bindgen --target web --no-typescript --out-name FrankenTerm --out-dir "$out/site/pkg" \
  "$target/renderer/wasm32-unknown-unknown/release/frankenterm_web.wasm"

(cd "$repo" &&
  cargo build --locked --release --lib --target wasm32-unknown-unknown \
    --target-dir "$target/viewer" -p frankenterm-browser-ui)
wasm-bindgen --target web --no-typescript --out-name ft_fleet_viewer --out-dir "$out/site/pkg" \
  "$target/viewer/wasm32-unknown-unknown/release/frankenterm_browser_ui.wasm"

cp "$crate_dir/web/index.html" "$crate_dir/web/app.mjs" "$crate_dir/web/app.css" "$out/site/"
cp "$out/renderer-source/fonts/pragmasevka-nf-subset.woff2" "$out/site/fonts/"
cp "$out/renderer-source/LICENSE" "$out/site/RENDERER-LICENSE"
cp "$repo/LICENSE" "$out/site/LICENSE"

# The manifest binds the served set to its inputs. ft web loads only the files
# it lists, after checking each digest.
digest() { shasum -a 256 "$1" | awk '{ print $1 }'; }
{
  printf '{\n  "schema": "ft-browser-ui-package-v1",\n'
  printf '  "toolchain": "%s",\n  "wasm_bindgen": "%s",\n' "$toolchain" "$bindgen_version"
  printf '  "renderer": {"revision": "%s", "archive_sha256": "%s", "lock_sha256": "%s"},\n' \
    "$renderer_revision" "$renderer_archive_sha256" "$(digest "$renderer_lock")"
  printf '  "viewer_lock_sha256": "%s",\n  "files": {\n' "$(digest "$repo/Cargo.lock")"
  first=1
  while IFS= read -r file; do
    rel=${file#"$out/site/"}
    [[ $first -eq 1 ]] || printf ',\n'
    first=0
    printf '    "%s": "%s"' "$rel" "$(digest "$file")"
  done < <(find "$out/site" -type f ! -name manifest.json | LC_ALL=C sort)
  printf '\n  }\n}\n'
} > "$out/site/manifest.json"

printf 'build-browser-ui: package at %s/site\n' "$out"
