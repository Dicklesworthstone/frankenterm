# GPU Regression Harness — Design Decisions

**Status:** Proposed
**Bead:** ft-ombfl.1 (parent: ft-ombfl)
**Predecessor:** ft-1memj.28 (divider geometry shipped as bf9db5d5)
**Date:** 2026-04-28

## Purpose

Specify the framework, capture path, on-disk format, comparator, fixture
layout, and CI strategy for a golden-image regression harness covering
`frankenterm-gui`'s render pipeline. This document is the blocker for
all ft-ombfl implementation children (ft-ombfl.2 through ft-ombfl.13);
they may begin once this is accepted.

Each numbered section below ends with a single **Decision** line. The
implementation children cite that decision when implementing.

---

## 1. Framework choice

**Options considered**

| # | Option | Pros | Cons |
|---|--------|------|------|
| A | Reuse `frankenterm-gui`'s wgpu render path on an offscreen surface | Tests exactly what ships; readback machinery already exists in `crates/frankenterm-gui/src/termwindow/webgpu.rs` (`WebGpuTexture::read`, `validate_texture_readback_request`, `padded_readback_bytes_per_row`) | Requires a windowless wgpu adapter; CI must have a usable backend (Metal on macOS, swiftshader/llvmpipe on Linux) |
| B | Native `frankentui` render with software fallback | Zero GPU dependency; portable | Doesn't catch shader/atlas/glyph-cache regressions which are the actual GUI failure mode; would test the wrong code path |
| C | Dedicated harness using mesa-swrast | Fully deterministic | Doubles implementation cost; the harness no longer exercises the production wgpu path |

The existing `WebGpuTexture::read()` already implements the texture →
mappable-buffer → CPU copy with the 256-byte row-alignment dance and a
5-second readback deadline. We do not have to invent a capture path; we
have to invoke it from a windowless context.

**Decision.** **Option A.** Reuse `frankenterm-gui`'s production wgpu
pipeline with an offscreen `wgpu::Texture` (no `wgpu::Surface`) as the
render target. CI uses Metal on macos-15 and **WebGPU's Vulkan/swiftshader
fallback on ubuntu-latest**, with a documented "soft" lane for
ubuntu-without-GPU that runs the harness in non-blocking mode (see §7).

## 2. Capture strategy

**Options considered**

| # | Option | Notes |
|---|--------|-------|
| A | `wgpu::Surface` swapchain readback | Requires a real window; not viable on headless CI without virtual display |
| B | Render-to-texture, `copy_texture_to_buffer`, `map_async` | Already implemented in `WebGpuTexture::read`. No surface needed |
| C | Frame-buffer dump via debug layer | Vendor-specific, not portable |

Option B is identical to the path the in-window readback uses today
(`WebGpuTexture::read`). Reusing it means the harness shares all the
hard-won subtleties: 256-byte row padding, 5s poll-loop, MapMode::Read
ordering. Diverging would mean re-deriving those.

The texture format must match the production `surface.get_capabilities`
output. Inspecting `webgpu.rs:535-577` shows the surface configures
itself from the adapter's preferred SRGB-aware format. The harness
**must use the same format selection logic** (extract into a shared
`pick_render_format(&adapter)` helper if not already public).

**Decision.** **Render-to-texture + `copy_texture_to_buffer` +
`map_async`,** invoking the existing `WebGpuTexture::read` machinery.
MSAA disabled in goldens (resolve before readback). Texture format
chosen by the same logic as the live surface; recorded into the
fixture's meta.json (see §3).

## 3. Golden format

**Options considered**

| Format | Size | Tooling | Diffability |
|--------|------|---------|-------------|
| PNG (lossless) | ~50–500 KB per fixture | Universal; `image` crate already in `frankenterm-gui` deps via wgpu's transitive `image_serde` | Easy: `git diff` shows nothing useful, but `oxipng diff` and image-diff tools work |
| QOI (lossless) | ~30% smaller | Thin tooling; reviewers can't preview | Pixel-exact but no GitHub preview |
| Raw RGBA8 | Fixed (W*H*4) | None; debug-only | Largest; useless for review |

PNG wins on tooling: GitHub previews diffs, `image` crate already in
the workspace, reviewers can drag-and-drop the file into any viewer.
Size cost is acceptable; 80% of expected fixtures are <300 KB at
typical terminal resolutions (e.g., 1280×800).

**PNG must be encoded deterministically.** The `image` crate's
`PngEncoder` writes a `tIME` chunk by default; left unchecked this
breaks reproducibility. The harness uses `PngEncoder::new_with_quality`
configured as:

- `CompressionType::Fast` (level 6 equivalent — pinned for stability)
- `FilterType::Adaptive` is non-deterministic across `image` versions;
  pin to `FilterType::Paeth` instead
- No `tEXt`/`iTXt`/`tIME` chunks; encode via the lower-level
  `png::Encoder` API directly to skip the `image` wrapper's metadata
  insertion

Each fixture has a sidecar `meta.json` capturing reproducibility
context that the PNG cannot:

```json
{
  "fixture": "cursor_blink_off",
  "viewport": { "width": 800, "height": 600, "dpi": 96.0 },
  "texture_format": "Bgra8UnormSrgb",
  "font_set_sha": "abc123…",
  "harness_version": 1,
  "generated_at_runner": "macos-15"
}
```

`generated_at_runner` is informational — the comparator does NOT key
off it (see §4) — but a mismatch helps explain a perceptual diff in PR
review.

**Decision.** **PNG (deterministic encoder config) + sidecar
`meta.json`.** Documented at `docs/design/gpu-regression-png-encoding.md`
follow-on.

## 4. Comparator algorithm

The comparator is the most consequential decision: too strict and the
harness flakes on every font-hinting jitter, too loose and real
regressions slip through.

**Options considered**

| # | Option | Catches | Misses |
|---|--------|---------|--------|
| A | Exact pixel match | Anything | Subpixel font-render differences across runner images, GPU drivers |
| B | Per-pixel L∞ tolerance (e.g., max channel-delta ≤ 4) | Subtle color shifts; resilient to 1-bit jitter | Whole-shape regressions if mean is preserved |
| C | SSIM ≥ 0.99 | Perceptual changes; tolerant of antialiasing | Tiny but localized regressions (e.g., 1px cursor offset on white background) |
| D | Combination: SSIM ≥ 0.99 **AND** L∞ ≤ 8 **AND** changed-pixel-count ≤ 0.1% of total | Both whole-image and localized | More moving parts |

Option D is the de-facto approach used by chromium-style image-diff
test harnesses. The three-clause AND covers complementary failure
modes:

- L∞ catches localized "cursor at the wrong pixel" regressions.
- SSIM catches perceptual whole-image regressions (e.g., a font
  switched to a different family).
- Changed-pixel-count catches "5 pixels are wildly wrong" cases that
  L∞ alone can hide if their channels happen to be similar.

Implementation: pull `image-compare` (or write a 100-line SSIM in
the harness — Wang et al. 2004 SSIM is short). L∞ and changed-pixel-
count are trivial. **Thresholds are configurable per-fixture** in the
sidecar `meta.json` so a notoriously flaky fixture (emoji color
glyphs) can be loosened without loosening everything.

When a comparison fails, the harness writes three artifacts next to
the fixture:

- `<fixture>.actual.png` — the just-rendered image
- `<fixture>.diff.png` — pixel-difference visualization (red = diff)
- `<fixture>.report.json` — `{ ssim, l_inf, changed_pixels, threshold }`

These are uploaded by CI on failure (see §7) so reviewers see the
diff without re-running the harness locally.

**Decision.** **Triple-clause comparator: SSIM ≥ 0.99 AND L∞ ≤ 8 AND
changed-pixel-fraction ≤ 0.001 (0.1%).** Per-fixture override allowed
via `meta.json`. Failure produces `<fixture>.{actual,diff,report}.*`
artifacts.

## 5. Test directory layout

```
crates/frankenterm-gui/tests/
├── golden/                          # NEW — covered by ft-ombfl.2
│   ├── fixtures/                    # NEW — covered by ft-ombfl.4
│   │   ├── cursor_blink_off/
│   │   │   ├── golden.png
│   │   │   ├── meta.json
│   │   │   └── scenario.lua         # how to drive the renderer
│   │   ├── selection_word_wrap/
│   │   └── …
│   ├── fonts/                       # NEW — pinned font set, see §6
│   │   └── MANIFEST.toml
│   └── README.md
├── golden_harness.rs                # NEW — covered by ft-ombfl.2 & .3
└── existing tests (unchanged)
```

The golden tests live under the `frankenterm-gui` crate (not the
workspace root) because they exercise that crate's render path. Other
crates' golden harnesses can mirror this pattern under their own
`tests/golden/` if needed, but this design only commits the
gui-renderer slice.

A fixture is a directory, not a single file — the renderer driver
(`scenario.lua` or equivalent) lives next to the golden so a reviewer
sees the input and output together.

**Decision.** **Per-fixture directory under
`crates/frankenterm-gui/tests/golden/fixtures/`** containing
`golden.png`, `meta.json`, and the scenario driver. Pinned fonts at
`crates/frankenterm-gui/tests/golden/fonts/`.

## 6. Reproducibility constraints

| Source of nondeterminism | Mitigation |
|--------------------------|------------|
| System fonts vary by OS minor version | Pin font set in `tests/golden/fonts/`; force the harness to load only those |
| GPU drivers vary by macOS version | Pin CI to `macos-15` runner image; document in this doc + `meta.json` |
| Locale (number formatting, calendar) | `LC_ALL=C` enforced by harness setup; reject if env says otherwise |
| Timezone (timestamp rendering) | `TZ=UTC` enforced by harness setup |
| `image` crate version | Pin via Cargo.lock; the harness asserts `image::version()` matches a recorded hash at startup |
| Time-of-day cursor blink phase | Disable blink in fixtures (set blink interval to 0 or freeze the clock) |
| Floating-point rounding across CPU microarchitectures | Tolerated by the §4 comparator (L∞ ≤ 8 ≈ 3% per-channel) |

**Font binaries** are 80 MB raw. We choose download-on-first-use over
bundling:

- `crates/frankenterm-gui/tests/golden/fonts/MANIFEST.toml` lists
  exact font name, version, SHA256, and stable URL.
- `crates/frankenterm-gui/tests/golden/fonts/fetch.sh` (and a
  `fetch.rs` cargo binary) downloads + validates checksums.
- The harness fails fast if the local cache is missing/corrupt;
  CI runs the fetch step before tests.

If upstream URLs become unstable, fall back to font subsetting via
`pyftsubset` (covered by ft-ombfl.4 follow-on; not in scope here).

**Decision.** **Pin everything that varies; tolerate microarchitecture
float drift via the comparator.** Font binaries managed via a
checksummed manifest + on-demand fetcher (not bundled).

## 7. CI strategy

Two CI lanes:

| Lane | Runner | Behavior on diff | Cost |
|------|--------|------------------|------|
| Hard gate | `macos-15` (Apple Silicon, Metal) | **Failure blocks merge.** Goldens valid against this image | Standard macOS minutes |
| Soft lane | `ubuntu-latest` (Vulkan/swiftshader fallback) | **Failure logs an artifact but does NOT block merge.** Used to detect drift between platforms | Cheap; runs in same workflow |

Rationale: Apple Silicon is the developer reference platform. Ubuntu
is for catching cross-platform regressions early without coupling
release readiness to mesa/swiftshader behavior we don't control.

When `macos-15` retires (GitHub typically pre-announces 6 months in
advance), a coordinated regen PR re-captures all goldens against the
new runner. Filed as a follow-on bead at that time.

CI step skeleton (jobs to be implemented in ft-ombfl.5+):

```yaml
- name: Fetch pinned fonts
  run: cargo run -p frankenterm-gui --bin fetch-fonts

- name: GPU regression harness (macos-15, hard gate)
  if: matrix.os == 'macos-15'
  run: cargo test -p frankenterm-gui --test golden_harness -- --test-threads=1

- name: Upload diff artifacts on failure
  if: failure()
  uses: actions/upload-artifact@v4
  with:
    name: gpu-harness-diffs-${{ matrix.os }}
    path: crates/frankenterm-gui/tests/golden/**/*.{actual,diff,report}.*
```

`--test-threads=1` is **mandatory**: the harness shares one wgpu
adapter; concurrent invocations will starve each other on the readback
poll loop.

**Decision.** **Two-lane CI: macos-15 hard-gates, ubuntu-latest
soft-warns.** Diff artifacts uploaded on failure for review.

---

## Implementation order

The decisions above unblock the following implementation chain:

1. **ft-ombfl.2** — Harness scaffold (the wgpu offscreen adapter,
   `golden_harness.rs`, the comparator, fixture loader).
2. **ft-ombfl.3** — Renderer integration (drive
   `frankenterm-gui`'s render path against an offscreen target).
3. **ft-ombfl.4** — Fixture authoring (one fixture per regression
   class: cursor, selection, scrollback, emoji-fallback, shaping).
4. **ft-ombfl.5** — CI workflow integration.
5. ft-ombfl.6 onward — fixture growth + lint integration.

A reviewer can begin ft-ombfl.2 the moment this doc is accepted.

## Acceptance test for this doc

A fresh reader, after 60 seconds reading, should answer:

- *What library compares images?* SSIM (custom 100-LOC implementation)
  + L∞ + changed-pixel-count, all in-tree under the harness.
- *Where do golden files live?*
  `crates/frankenterm-gui/tests/golden/fixtures/<name>/`.
- *Which CI lane gates merges?* macos-15.
- *Why PNG and not QOI?* Tooling: GitHub diff preview + image crate
  already in deps.

If any answer is unclear, that section needs a rewrite before
acceptance.

## Open questions left to children

- **Exact SSIM implementation** (in-tree vs `image-compare` crate) —
  decide in ft-ombfl.2 based on weight of dep tree.
- **Whether `scenario.lua` is the driver format or whether we use a
  Rust API** — decide in ft-ombfl.3 based on what the gui crate
  exposes for headless drive.
- **Linux GPU CI cost analysis** (covered by ft-ombfl.16, stretch).

These do not block ft-ombfl.2/3/4 starting.
