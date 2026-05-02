# SDF Glyph Atlas Evaluation

**Bead:** `ft-mpc9b.6.5`  
**Date:** 2026-05-02  
**Decision:** Defer shipping an SDF glyph atlas. Keep the bitmap atlas as the
default renderer path until a visual harness proves SDF quality at terminal text
sizes.

## Existing Renderer Facts

- `frankenterm/window/src/bitmaps/atlas.rs` owns the versioned bitmap atlas.
  The atlas records allocation, growth, packing efficiency, and rebuild
  telemetry, and existing resize work depends on its version cursor rather than
  replacing it during pure window-size changes.
- `crates/frankenterm-gui/src/glyphcache.rs` rasterizes glyphs through the
  current font rasterizer stack and uploads bitmap sprites into the atlas. Its
  cache key already distinguishes monochrome, variable, COLR/CPAL, and bitmap
  color glyph formats.
- `crates/frankenterm-gui/src/shader.wgsl` and
  `crates/frankenterm-gui/src/glyph-frag.glsl` sample atlas textures as bitmap
  color/alpha data through nearest and linear samplers. There is no current
  distance-field shader contract for per-glyph spread, threshold, or gamma.
- `docs/render/draft-mode.md` lists SDF glyphs as a future-quality feature that
  draft mode may disable, not as a shipped renderer path.

## Spike Assessment

SDF can reduce duplicate glyph textures across DPI and scale changes, but that
benefit is not yet proven to dominate FrankenTerm's current renderer cost. The
existing atlas path already has a no-rebuild pure-resize contract, and the
remaining SDF win would mostly come from sharing glyph assets across font sizes
and scale factors.

The quality risk is high for terminal workloads:

- Operators spend most of their time at small sizes, often 10-14 px. The bead's
  own risk section calls out blur below 14 px.
- Terminal text depends on stem sharpness, box drawing alignment, underlines,
  cursor adjacency, and dense colored spans. These are exactly where a naive SDF
  spread or threshold produces visible softness.
- Emoji, COLR/CPAL, bitmap strikes, and other color glyph formats cannot safely
  share the same monochrome SDF path. They need mandatory bitmap fallback.
- Subpixel and LCD rasterizer output is a bitmap contract today. Replacing it
  with grayscale SDF would also be a visual-policy change, not just an atlas
  storage change.

## Decision

Do not implement or enable SDF glyph rendering under `ft-mpc9b.6.5`. Shipping it
now would add a second glyph representation, shader branch, cache policy, and
quality surface without the required screenshot and SSIM evidence.

The safe design remains:

- Bitmap atlas is the default and required path.
- Any SDF experiment is opt-in and restricted to monochrome outline glyphs.
- Bitmap fallback is mandatory for font sizes below 14 px, emoji, COLR/CPAL,
  CBDT/CBLC, sbix, bitmap strikes, and any glyph whose SDF golden is rejected.
- Atlas entries must record representation metadata before SDF can coexist with
  current bitmap sprites.
- Draft mode may later disable SDF as a cosmetic feature, but draft-mode policy
  alone is not evidence that the SDF path is visually acceptable.

## Required Evidence Before Enabling

Before this can move from research to shipping work, a follow-up bead needs all
of the following:

1. A renderer golden corpus covering ASCII, box drawing, powerline separators,
   combining marks, ligatures disabled/enabled, emoji/color glyphs, bold, italic,
   and high-contrast foreground/background pairs.
2. Bitmap-vs-SDF captures at 10, 12, 14, 16, 18, 24, and 32 px, including
   standard and HiDPI scale factors.
3. SSIM >= 0.97 for SDF-eligible glyphs at 14 px and above, plus a manual
   rejection checklist for stem blur, box-drawing seams, underline position, and
   color fringe artifacts.
4. Bench output comparing bitmap and SDF glyph raster time, atlas upload bytes,
   atlas residency bytes, and shader cost under a resize/DPI-change storm.
5. A hybrid fallback proof showing that non-SDF glyph formats keep stable cache
   keys and cannot accidentally enter the SDF shader path.

## Follow-Up Shape

If the evidence above passes, the implementation should land behind a disabled
experiment flag first:

- Add explicit glyph representation metadata to atlas/cache entries.
- Generate distance fields only for eligible monochrome outline glyphs.
- Add WGSL/GLSL sampling parameters for spread and threshold.
- Preserve the current bitmap code path as the default and fallback.
- Add renderer goldens before enabling the flag by default.

That keeps `ft-mpc9b.6.5` as a decision artifact and prevents a research-grade
renderer idea from silently becoming a default text-quality regression.
