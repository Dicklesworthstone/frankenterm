# Renderer Bitmap Corpus Contract

Bead: `ft-tf6g3.33`

This contract defines the frozen bitmap substrate used by renderer parity
tests. It is separate from the live GPU golden harness under
`tests/golden/gpu/`: the harness can keep evolving, while this corpus preserves
bit-exact frame bytes with enough sidecar metadata to explain and regenerate
each frame.

## Layout

Corpus frames live under:

```text
tests/fixtures/renderer-corpus/<group>/<scenario>/<frame>.png
tests/fixtures/renderer-corpus/<group>/<scenario>/<frame>.json
```

Path components are lowercase kebab-case identifiers. Each `*.png` must have a
sibling `*.json` with the same basename. Directory names are part of the
identity and must match the sidecar's `group`, `scenario`, and `frame` fields.

The seed that declares frame sequences lives at:

```text
tests/fixtures/renderer-corpus/seed.yaml
```

## Frame Sidecars

Each frame sidecar is deterministic JSON with this minimum shape:

```json
{
  "schema_version": "renderer-corpus-frame.v1",
  "group": "smoke",
  "scenario": "static-png-roundtrip",
  "frame": "frame-000",
  "source_png": "tests/golden/gpu/_smoketest/golden.png",
  "viewport": {
    "width_px": 64,
    "height_px": 64,
    "scale_factor": 1.0
  },
  "monitors": [
    {
      "id": "main",
      "origin_x": 0,
      "origin_y": 0,
      "width_px": 64,
      "height_px": 64,
      "scale_factor": 1.0
    }
  ],
  "cursor": null,
  "selection": null,
  "png_compression": {
    "color_type": "rgba8",
    "bit_depth": 8,
    "interlace": "none",
    "encoder": "source-bytes",
    "zlib_level": "preserved",
    "filter": "preserved"
  },
  "content_hash": "sha256:<64 lowercase hex chars>",
  "seed": {
    "path": "tests/fixtures/renderer-corpus/seed.yaml",
    "scenario_revision": 1
  }
}
```

Required sidecar fields:

- `viewport`: pixel dimensions and scale factor used for the frame.
- `monitors`: the monitor geometry visible to the renderer for the frame.
- `cursor`: cursor state, or `null` when no cursor is asserted.
- `selection`: selection state, or `null` when no selection is asserted.
- `png_compression`: the byte-level PNG policy used by the generator.
- `content_hash`: `sha256:` plus the SHA-256 of the sibling PNG bytes.

## PNG Policy

Corpus PNGs are immutable artifacts once committed. A frame refresh must update
the PNG and sidecar in the same commit. The sidecar hash is the source of truth:
changing a PNG without updating the sibling JSON is a CI failure.

The canonical bitmap policy is:

- RGBA8 pixels.
- 8-bit channels.
- no interlace.

When a corpus frame is sourced from an existing checked-in golden, the generator
copies the source bytes exactly and records `encoder: "source-bytes"`,
`zlib_level: "preserved"`, and `filter: "preserved"`. The source file must
already be tracked in the repository, and the sidecar `content_hash` pins the
exact bytes.

A future native renderer exporter may write fresh PNG bytes. That exporter must
use deterministic zlib level 9 output, adaptive PNG filters, and no timestamps,
host paths, or volatile text chunks. It must also update this contract and the
generator before landing new frames.

## YAML Seed

The seed declares the frame sequence. The generator only accepts the constrained
schema below, even though the file syntax is YAML:

```yaml
schema_version: renderer-corpus-seed.v1
output_root: tests/fixtures/renderer-corpus
png_compression:
  color_type: rgba8
  bit_depth: 8
  interlace: none
  encoder: source-bytes
  zlib_level: preserved
  filter: preserved
groups:
  - id: smoke
    scenarios:
      - id: static-png-roundtrip
        revision: 1
        viewport:
          width_px: 64
          height_px: 64
          scale_factor: 1.0
        monitors:
          - id: main
            origin_x: 0
            origin_y: 0
            width_px: 64
            height_px: 64
            scale_factor: 1.0
        frames:
          - id: frame-000
            source_png: tests/golden/gpu/_smoketest/golden.png
            cursor: null
            selection: null
```

Regenerate or check the corpus with:

```bash
scripts/regenerate-renderer-corpus.sh
scripts/regenerate-renderer-corpus.sh --check
scripts/check-renderer-corpus-drift.sh
```

The generator refuses to overwrite changed existing files unless `--force` is
passed. Use `--check` in review lanes to prove the committed corpus still
matches the seed.

## Drift Rules

`scripts/check-renderer-corpus-drift.sh` enforces:

- every corpus PNG has a sibling JSON sidecar;
- every corpus JSON sidecar has a sibling PNG;
- every sidecar has the required viewport, monitor, cursor, selection,
  compression, and hash fields;
- every sidecar `content_hash` equals the current sibling PNG SHA-256;
- if Git diff context is available, a changed corpus PNG requires its sibling
  JSON sidecar to be changed in the same diff.

These rules make the corpus reproducible without trusting file mtimes, local
renderer state, or unstated fixture assumptions.
