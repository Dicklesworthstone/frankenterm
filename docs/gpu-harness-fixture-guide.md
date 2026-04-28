# GPU Harness Fixture Guide

This guide explains how to add a new visual regression fixture to the
`frankenterm-gui` GPU golden-image harness.

The harness fixture root is `tests/golden/gpu/`. Each fixture is one directory:

```text
tests/golden/gpu/<fixture-name>/
|-- input.json
|-- meta.json
|-- expected.json
`-- golden.png
```

Use kebab-case fixture names and keep the name tied to the regression class:
`text-basic-paragraph`, `text-cjk-mixed`, `cursor-block-steady`,
`selection-word`, and `text-box-drawing` are good examples.

## Pick The Scene

Start from a baseline scene that isolates one failure mode. A good fixture is
small, readable, and boring outside the thing it is meant to catch.

Choose:

- a viewport size that gives enough context without making the PNG huge;
- deterministic text content, with no timestamps or local paths;
- a single primary visual behavior: wrapping, CJK fallback, cursor shape,
  selection highlight, box drawing, or overlay tint;
- a cursor or selection only when it is part of the regression class.

Recent examples:

- `ft-ombfl.4`: `text-basic-paragraph` covers ASCII text, wrapping, blank
  lines, and a ruler line.
- `ft-ombfl.5`: `text-cjk-mixed`, `text-combining-marks`,
  `text-emoji-fallback`, and `text-rtl-arabic-hebrew` cover Unicode and
  fallback classes.
- `ft-ombfl.6`: `text-box-drawing` covers table joins, block elements, and
  shading.
- `ft-ombfl.7`: `cursor-block-steady`, `cursor-beam-blink`, and related
  cursor fixtures cover cursor shape and deterministic blink-state rendering.
- `ft-ombfl.8`: `selection-char`, `selection-word`, `selection-line`,
  `overlay-ime-composition`, and `overlay-visual-mode` cover selection and
  overlay blending.

## Write `input.json`

For normal visual fixtures use `headless_terminal`:

```json
{
  "kind": "headless_terminal",
  "lines": [
    "Fixture title: describe the regression class.",
    "Small deterministic content goes here.",
    "Keep neighboring rows as context for bleed checks."
  ],
  "cursor": {
    "row": 1,
    "col": 12,
    "shape": "block"
  },
  "selection": {
    "start_row": 1,
    "start_col": 6,
    "end_row": 1,
    "end_col": 18
  },
  "cursor_blink_disabled": true,
  "ime_disabled": true
}
```

`cursor` and `selection` are optional. Valid cursor shapes are `block`,
`underline`, and `beam`. Selection bounds are cell coordinates, inclusive at
both ends.

Keep `cursor_blink_disabled` and `ime_disabled` explicit unless the fixture is
specifically testing deterministic blink-state or IME/preedit overlay behavior.

## Write `meta.json`

Use the current default comparator thresholds unless there is a documented
reason to tighten or loosen them:

```json
{
  "fixture": "selection-word",
  "viewport": {
    "width": 640,
    "height": 336,
    "dpi": 96.0
  },
  "texture_format": "Rgba8UnormSrgb",
  "font_set_sha": "headless-rasterizer-v1-no-font-fetch",
  "harness_version": 1,
  "generated_at_runner": "macos-metal-cod_3-2026-04-28",
  "thresholds": {
    "min_ssim": 0.99,
    "max_l_inf": 8,
    "max_changed_pixel_fraction": 0.001
  }
}
```

The comparator requires all three checks to pass:

- `ssim >= min_ssim`
- `l_inf <= max_l_inf`
- `changed_pixel_fraction <= max_changed_pixel_fraction`

If a fixture needs a looser threshold, record why in the commit message or bead
notes. Do not loosen thresholds to hide unexplained drift.

## Write `expected.json`

Most fixtures use:

```json
{
  "status": "pass"
}
```

Keep `expected.json` small. The visual contract is the PNG plus metrics; do not
duplicate the fixture scene in this file.

## Capture `golden.png`

Generate goldens with the explicit update gate:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-cod_3-target SET_GOLDEN=1 \
  cargo test -p frankenterm-gui --features headless-render \
  --test gpu_regression -- --update-goldens --nocapture
```

The harness currently discovers the full fixture tree, so this may update or
validate fixtures owned by other active panes. Stage only the fixture directory
you own.

After generation, inspect the JSON-line output for your fixture:

```json
{"name":"selection-word","phase":"fixture","render_ms":24,"compare_ms":54,"ssim":1.0,"linf":0,"changed_pixel_fraction":0.0,"status":"pass"}
```

For a freshly generated golden, `ssim` should be `1.0`, `linf` should be `0`,
and `changed_pixel_fraction` should be `0.0`. If not, stop and inspect the
fixture before committing.

## Validate Without Updating

Run the harness again without `SET_GOLDEN=1` and without `--update-goldens`:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-cod_3-target \
  cargo test -p frankenterm-gui --features headless-render \
  --test gpu_regression -- --nocapture
```

The summary must report zero failures. If a fixture fails, inspect the artifact
root shown in the output. By default, failure artifacts go under
`target/gpu-regression/`; `GPU_HARNESS_ARTIFACT_DIR` can override that path.

## Commit Only The Fixture Contract

Commit the complete fixture directory:

- `input.json`
- `meta.json`
- `expected.json`
- `golden.png`

`meta.json` is ignored by the repo-wide ignore rules, so add it explicitly:

```bash
git add tests/golden/gpu/<fixture-name>/input.json \
  tests/golden/gpu/<fixture-name>/expected.json \
  tests/golden/gpu/<fixture-name>/golden.png
git add -f tests/golden/gpu/<fixture-name>/meta.json
git commit -m "test(gpu): add <fixture-name> golden fixture (<bead-id>)" -- \
  tests/golden/gpu/<fixture-name>
```

In a shared swarm checkout, check `git diff --cached --name-only` before
committing. Do not commit another pane's fixture directories, generated
artifacts, or unrelated `.beads` changes.

## Updating Existing Goldens

Only update an existing `golden.png` when the visual behavior intentionally
changed. The commit should explain the behavior change, not just say the golden
was refreshed.

Before committing:

- run with `SET_GOLDEN=1 --update-goldens`;
- run again without updating;
- inspect the generated image and any diff report;
- keep thresholds unchanged unless the visual contract itself changed.

For a real renderer bug, add or update the fixture in the same commit as the
fix when possible. The fixture should fail before the fix and pass after it.
