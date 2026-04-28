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

`input.json` describes how the harness obtains the actual frame. The
scaffold supports `static_png_roundtrip`, which loads `golden.png` back
through the fixture loader so comparator and artifact behavior can be
tested before renderer integration lands.

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
{"phase":"summary","total":1,"passed":1,"failed":0}
```

Goldens can be re-pinned only with both the CLI flag and explicit
confirmation:

```bash
SET_GOLDEN=1 cargo test -p frankenterm-gui --test gpu_regression -- --update-goldens
```

The `_smoketest` fixture is intentionally renderer-free. It validates
real PNG decode, fixture metadata, comparator metrics, and diff-PNG
generation while later beads wire in offscreen GPU rendering.
