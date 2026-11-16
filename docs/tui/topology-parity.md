# TUI Topology Parity

**Bead:** `ft-tf6g3.24`
**Crate:** `crates/frankenterm-topo`
**Release slot:** `docs/attestations/tui/topology-parity.json`

SSIM and pixel-difference gates catch byte-level visual drift. They do
not directly ask whether a rendered glyph still has the same connected
components and enclosed holes. The topology parity lane adds that check
for small glyph bitmaps by computing H0/H1 Betti curves over an ink
super-level filtration and comparing persistence diagrams with the
bottleneck metric.

The implementation is intentionally small and deterministic:

- input is a grayscale glyph bitmap, normally an alpha or luma plane
- thresholds descend through nonzero pixel values
- foreground uses 4-connected components
- H1 is counted as background components that do not touch the image
  border
- per-dimension bottleneck distances are computed with exact bipartite
  matching against diagonal features

For release gating, each glyph comparison records the SSIM result from
the render-parity lane and the H0/H1 bottleneck distances from
`frankenterm-topo`. A glyph passes the topology adjunct only when both
H0 and H1 distances stay below its configured threshold.

The checked-in attestation is substrate evidence. It proves the crate,
metric, thresholds, and bundle slot exist; it does not claim a production
oracle-vs-subject render run until the renderer harness emits concrete
glyph bitmaps for both sides of every terminal-conformance fixture.
