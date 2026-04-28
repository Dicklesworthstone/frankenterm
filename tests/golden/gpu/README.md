# GPU Golden Fixture Layout

This directory holds golden-image fixture data for the
`frankenterm-gui` GPU regression harness.

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

`meta.json` records deterministic rendering context and per-fixture
thresholds. The default comparator contract is:

- `ssim >= 0.99`
- `l_inf <= 8`
- `changed_pixel_fraction <= 0.001`

`expected.json` declares the expected fixture status. Failure artifacts
are written outside the fixture tree, under `GPU_HARNESS_ARTIFACT_DIR`
when set, otherwise `target/gpu-regression/`.

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
