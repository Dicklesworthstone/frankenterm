# GPU Regression Harness — Design Decisions

**Status:** Historical design; headless harness/artifact path implemented,
native renderer qualification incomplete
**Bead:** ft-ombfl.1 (parent: ft-ombfl)
**Predecessor:** ft-1memj.28 (divider geometry shipped as bf9db5d5)
**Date:** 2026-04-28

**Current source boundary (2026-09-04):** the runnable target is
`crates/frankenterm-gui/tests/gpu_regression.rs`, with fixtures under
`tests/golden/gpu/`. It supports static PNG roundtrips, feature-gated
headless rendering, and fuzz runs. These scopes do not establish live
`TermWindow` equivalence, native presentation, or screen-reader delivery.
The current coverage and native acceptance authority is
[renderer-scenario-contract.md](renderer-scenario-contract.md).
The decisions below retain the original design intent; they are not a
claim that every extraction, driver, or reproducibility control landed.

## Purpose

Specify the framework, capture path, on-disk format, comparator, fixture
layout, and verification strategy for a golden-image regression harness covering
`frankenterm-gui`'s render pipeline. This document is the blocker for
all ft-ombfl implementation children (ft-ombfl.2 through ft-ombfl.13);
they may begin once this is accepted.

Each numbered section below ends with a single **Decision** line. The
implementation children cite that decision when implementing.

The shipped wrapper, `scripts/test-gpu-harness.sh`, emits
`summary.json` and `render-parity-gpu.json` for each run. The latter is
the machine-readable visual-parity artifact for release attestation; its
checked-in contract is
`docs/attestations/tui/render-parity-gpu.json`.

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
render target. This is the design target; the current headless path is
not full production-path equivalence. Metal and software-adapter evidence
must be identified separately, using the RCH/DSR boundaries in §7.

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

**PNG must be encoded deterministically.** Pin the encoder dependency
and settings, keep run timestamps in sidecar metadata, and verify repeated
encoding of identical pixels. The original claims about automatic `tIME`
insertion and compression/filter behavior were not supported by retained
source evidence and are not part of this contract.

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

Retain these with the RCH/DSR run (see §7) so reviewers can inspect
the diff without re-running the harness.

**Decision.** **Triple-clause comparator: SSIM ≥ 0.99 AND L∞ ≤ 8 AND
changed-pixel-fraction ≤ 0.001 (0.1%).** Per-fixture override allowed
via `meta.json`. Failure produces `<fixture>.{actual,diff,report}.*`
artifacts.

## 5. Test directory layout

```
tests/golden/gpu/<fixture-name>/
├── input.json
├── golden.png
├── meta.json
└── expected.json

crates/frankenterm-gui/tests/gpu_regression.rs
```

The harness target belongs to `frankenterm-gui`; its fixture data lives
at the workspace root. `input.json` declares the actual frame source,
including renderer-free `static_png_roundtrip` and `headless_terminal`.

A fixture is a directory, not a single file — the renderer driver
(`input.json`) lives next to the golden so a reviewer
sees the input and output together.

**Current layout.** **Per-fixture directory under `tests/golden/gpu/`**.
See [the fixture guide](../gpu-harness-fixture-guide.md) for the authoring
contract. The pinned-font system below is the original design requirement,
not proof that its proposed paths or fetch commands exist.

## 6. Reproducibility constraints

| Source of nondeterminism | Mitigation |
|--------------------------|------------|
| System fonts vary by OS minor version | Pin font set in `tests/golden/fonts/`; force the harness to load only those |
| GPU drivers vary by macOS version | Record and qualify the native DSR host, OS, driver, and adapter in the evidence |
| Locale (number formatting, calendar) | `LC_ALL=C` enforced by harness setup; reject if env says otherwise |
| Timezone (timestamp rendering) | `TZ=UTC` enforced by harness setup |
| `image` crate version | Pin via Cargo.lock and retain source/dependency identity; startup hash enforcement must be demonstrated separately |
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

## 7. RCH development proof and DSR release qualification

FrankenTerm prohibits GitHub Actions. Development Cargo checks run through
strict remote RCH; releases and native release qualification use DSR.
This document does not establish an automatically configured GPU gate or
a scheduled fuzz run. Inspect the current DSR configuration and retained
receipts before attributing any result to a release.

Retain source identity, host/OS, adapter/backend, fixture inputs and counts,
`render-parity-gpu.json`, `summary.json`, `events.jsonl`, and every failure
PNG/report. A missing adapter is infrastructure failure, and an empty or
skipped fixture set is unproven. Static PNG roundtrips establish comparator
behavior; headless software and Metal runs establish only their recorded
paths. Native presentation and full production rendering require the
separate scenario contract's driver and capture evidence.

**Decision.** Qualify each declared path on its actual RCH/DSR host, with
nonzero applicable fixtures and retained failure artifacts. Preserve
reference goldens until an explicitly reviewed recapture is authorized.

---

## Original implementation order (historical)

The original plan used the following implementation chain:

1. **ft-ombfl.2** — Harness scaffold (the wgpu offscreen adapter,
   `golden_harness.rs`, the comparator, fixture loader).
2. **ft-ombfl.3** — Renderer integration (drive
   `frankenterm-gui`'s render path against an offscreen target).
3. **ft-ombfl.4** — Fixture authoring (one fixture per regression
   class: cursor, selection, scrollback, emoji-fallback, shaping).
4. **ft-ombfl.5** — Verification integration (historical breakdown;
   current release orchestration must use DSR).
5. ft-ombfl.6 onward — fixture growth + lint integration.

Current work must use the source status and native scenario contract above.

## Acceptance test for this doc

A fresh reader, after 60 seconds reading, should answer:

- *What library compares images?* SSIM (custom 100-LOC implementation)
  + L∞ + changed-pixel-count, all in-tree under the harness.
- *Where do golden files live?*
  `tests/golden/gpu/<name>/`.
- *What qualifies a release?* Retained applicable DSR evidence; development
  Cargo proof uses strict remote RCH. No Actions lane is authoritative.
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
