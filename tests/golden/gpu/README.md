# GPU Golden Fixture Layout

This directory holds golden-image fixture data for the
`frankenterm-gui` GPU regression harness.

For the authoring workflow, see
[GPU Harness Fixture Guide](../../../docs/gpu-harness-fixture-guide.md).

Each fixture is a directory:

```text
tests/golden/gpu/<fixture-name>/
├── input.json
├── golden.png
├── meta.json
└── expected.json
```

`input.json` describes how the harness obtains the actual frame.

- `static_png_roundtrip` loads `golden.png` back through the fixture
  loader so comparator and artifact behavior can be tested without GPU
  readiness.
- `headless_terminal` calls the feature-gated
  `frankenterm_gui::headless_render::render_headless` entrypoint. That
  path renders into an offscreen `wgpu::Texture`, reads back tightly
  packed RGBA8 pixels, and emits `render-frame` JSON-line metadata. It
  requires `cargo test -p frankenterm-gui --features headless-render
  --test gpu_regression`.

- `gui_snapshot` renders through the real `frankenterm-gui` binary
  (ft-yccm0.1.10). `input.scene` holds the bytes played into a fresh pane
  plus configuration changes; `frankenterm_gui::render_corpus` launches the
  GUI with a pinned configuration (bundled JetBrains Mono, Fira Code, Noto
  Color Emoji and Symbols Nerd Font from `frankenterm/assets/fonts`, macOS
  Hiragino Sans GB and Apple SD Gothic Neo for CJK, 13 pt at 144 DPI, no
  blinking, WebGpu) in a throwaway `HOME`, with a cleared environment,
  `--always-new-process`, and `FRANKENTERM_NATIVE_E2E_NONACTIVATING=1`, so
  it never touches a running mux or takes keyboard focus. The scene ends by
  setting the title `ft-render-snapshot`; the GUI's
  `FRANKENTERM_RENDER_SNAPSHOT` hook then copies the exact texture it is
  about to present into a PNG. That is the production font, shaping, glyph
  atlas and shader output. The `headless_terminal` kind above, by contrast,
  draws with a synthetic CPU rasterizer. These fixtures live under `real/`,
  need macOS with a display, and run only when named or when
  `FT_GPU_HARNESS_REAL_RENDERER=1` is set.

Real-renderer corpus commands (macOS, native; the GUI cannot build on the
Linux workers):

```bash
# Compare every real-renderer scene against its golden.
GPU_HARNESS_FIXTURE_FILTER=real cargo test -p frankenterm-gui --test gpu_regression
# Re-pin after an intended rendering change (review every heatmap first).
SET_GOLDEN=1 GPU_HARNESS_FIXTURE_FILTER=real \
  cargo test -p frankenterm-gui --test gpu_regression -- --update-goldens
```

`FT_GPU_HARNESS_GUI_BIN` points the harness at a different GUI build, such
as a release-interactive binary. Every run writes
`<artifact dir>/parity-receipt.json` with per-scene metrics. A failing
scene also gets `<name>.actual.png`, `<name>.diff.png`,
`<name>.heatmap.png` (per-pixel max-channel delta) and `<name>.report.json`.
Each GUI run's config, scene bytes, log and snapshot are kept under
`<artifact dir>/gui-snapshot/<name>/`.

A pinned `gui_snapshot` golden's `meta.json` records the snapshot size
(which depends on the display's backing scale), the DPI, `font_files` (path
and SHA-256 of every font the scene can resolve), `font_set_sha` over them,
`rasterizer`, and the GUI's own `Renderer initialized:` line. When such a
scene fails, its report carries `explained_delta`: which font files changed
since the golden was pinned, and whether the rasterizer changed (for
example FreeType to CoreText in Track C). Real-renderer scenes use the
parity thresholds `min_ssim >= 0.995` (mean of 8x8-window SSIM),
`min_window_ssim >= 0.95` (the worst window, which catches a regression
confined to a few glyphs), `l_inf <= 8`, and
`changed_pixel_fraction <= 0.001`.

`meta.json` records deterministic rendering context and per-fixture
thresholds. The default comparator contract is:

- `ssim >= 0.99`
- `l_inf <= 8`
- `changed_pixel_fraction <= 0.001`

`expected.json` declares the expected fixture status. Failure artifacts
are written outside the fixture tree, under `GPU_HARNESS_ARTIFACT_DIR`
when set, otherwise `target/gpu-regression/`.

CI and local pilot lanes can narrow or relocate the fixture set:

- `GPU_HARNESS_FIXTURE_FILTER=a,b,c` runs only the named fixtures.
- `GPU_HARNESS_FIXTURE_ROOT=/path/to/gpu-goldens` reads and updates a
  separate fixture namespace.
- `FT_GPU_HARNESS_FORCE_SOFTWARE=1` asks `wgpu` for a fallback software
  adapter.
- `FT_GPU_HARNESS_EXPECT_SOFTWARE=1` fails the run unless adapter metadata
  identifies a CPU/software renderer.
- `FT_GPU_HARNESS_EXPECT_ADAPTER_SUBSTRING=llvmpipe` pins a pilot lane to a
  specific adapter marker.

Development Cargo proof uses strict remote RCH; release orchestration and
qualification use DSR exclusively. Retain source identity, fixture counts,
adapter/backend, and the complete run artifacts. No Actions branch-protection
check is a qualification surface for FrankenTerm. A static PNG roundtrip,
software-adapter render, or offscreen Metal render establishes only that
recorded path; native presentation uses the separate renderer scenario contract.

The harness emits JSON-line events to stderr:

```json
{"phase":"discover","count":1}
{"phase":"fixture","name":"_smoketest","status":"start"}
{"phase":"fixture","name":"_smoketest","render_ms":1,"compare_ms":1,"status":"pass"}
{"phase":"render-frame","name":"ascii-basic","ms":3,"glyphs":18,"texture_format":"Rgba8UnormSrgb"}
{"phase":"summary","total":1,"passed":1,"failed":0}
```

Goldens can be re-pinned only with both the CLI flag and explicit
confirmation:

```bash
SET_GOLDEN=1 cargo test -p frankenterm-gui --test gpu_regression -- --update-goldens
```

The `_smoketest` fixture is intentionally renderer-free. It validates
real PNG decode, fixture metadata, comparator metrics, and diff-PNG
generation while GPU readiness remains optional for scaffold checks.

Renderer integration can be probed explicitly:

```bash
cargo test -p frankenterm-gui --features headless-render --test gpu_regression -- --headless-render-self-test
```

If no usable GPU backend is available, the harness exits with code `2`
and reports the init failure as infrastructure, not as a golden
regression.

For the retained wrapper run on an authorized RCH/DSR execution host:

```bash
scripts/test-gpu-harness.sh
```

The wrapper creates `/tmp/gpu-harness-<timestamp>/`, captures the full
run in `run.log`, extracts structured harness events to `events.jsonl`,
collects failure `*.actual.png`, `*.diff.png`, and `*.report.json`
artifacts into `diffs/`, writes `summary.json` plus the attestation-
oriented `render-parity-gpu.json`, and prints a concise stdout summary.
To pass arguments through to the harness, place them after `--`:

```bash
scripts/test-gpu-harness.sh -- --headless-render-self-test
```
