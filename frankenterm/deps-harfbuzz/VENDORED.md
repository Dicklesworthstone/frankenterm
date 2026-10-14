# Vendored HarfBuzz

`harfbuzz/` is HarfBuzz 14.6.0 from https://github.com/harfbuzz/harfbuzz, tag `14.6.0`
(tag archive sha256 `eca8581e5ad8e21dbda31556c0363f764b1a754c9c1bb33ca7cd56ca1c07ddc9`).
`build.rs` compiles only `harfbuzz/src/harfbuzz.cc` (C++11, atomics and locks kept on).

Import policy: the tag's tree minus the directories the `.gitignore` names (`.ci/`,
`.circleci/`, `.github/`, `docs/`, `perf/`, `subprojects/`, `test/`, `util/`), with
FrankenTerm's `.gitignore`. Up to 11.2.1 those directories were tracked although the
`.gitignore` excluded them; the 14.6.0 import follows the `.gitignore`.

Local changes, both in Rust sources the build does not compile, kept from the 11.2.1 import:
- `src/rust/font.rs`: a poisoned font mutex is recovered (`clear_poison`, `into_inner`)
  instead of unwrapped (d899b3048).
- `src/wasm/rust/harfbuzz-wasm/src/lib.rs`: `.zip(positions)` instead of
  `.zip(positions.into_iter())`.

Bindings (`src/lib.rs`) are unchanged from 11.2.1: bindings generated from the 11.2.1 and
14.6.0 headers with the same bindgen (0.73.2, `regenerate.sh` flags) differ only by
additions (new scripts, draw and font APIs). Regenerate with `regenerate.sh` to use them.

To update: replace `harfbuzz/` with the new tag minus the directories above, reapply the
local changes, compare bindings generated from old and new headers, and run the GPU
snapshot harness with `FT_GPU_HARNESS_REAL_RENDERER=1` (ligature, combining-mark, CJK
and emoji scenes shape through it).
