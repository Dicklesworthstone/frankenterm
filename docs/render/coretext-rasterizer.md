# CoreText glyph rasterizer (macOS)

ft-yccm0.4.3.1. On macOS, glyph bitmaps come from CoreText by default
(`font_rasterizer = "CoreText"`). FreeType remains the rasterizer on other
platforms and the kill switch on macOS (`font_rasterizer = "FreeType"`). The
code is `frankenterm/font/src/rasterizer/coretext.rs`.

## What CoreText draws

Each glyph is drawn with `CTFontDrawGlyphs` into a `CGBitmapContext` sized to
its ink bounds:

- Outline glyphs go into an 8-bit DeviceGray context, drawn white on black.
  The gray value is the coverage, stored like FreeType's grayscale glyphs: an
  sRGB-encoded gray, with linear coverage as alpha.
- Color fonts (Apple Color Emoji's sbix bitmaps, COLR version 0) go into a
  premultiplied sRGB RGBA context.
- Antialiasing is on. Font smoothing (stem darkening) is allowed but off,
  matching Ghostty's default; its `font-thicken` is opt-in.
- Synthetic italic is a text-matrix skew of 0.2. Synthetic bold strokes the
  outline at ppem / 24, about FreeType's emboldening width.
- Advances and positions still come from the HarfBuzz shaper. The
  rasterizer only produces bitmaps and their bearings, so the metrics
  plumbing is the same as with FreeType. Unit tests compare CoreText
  advances with HarfBuzz shaping, and CoreText bitmap geometry with
  FreeType's.

## Faces that fall back to FreeType

`font_rasterizer = "CoreText"` asks CoreText for each face. A face it cannot
draw faithfully gets FreeType, and an info log line names it:

| Face | Why |
| --- | --- |
| COLR version 1 (the bundled Noto Color Emoji) | CoreText draws COLRv1 paint graphs blank. Measured: every emoji of the emoji scene was empty. FreeType paints COLR through the HarfBuzz COLR painter. |
| CBDT/CBLC bitmaps without sbix or COLR v0 | CoreText does not draw CBDT. |
| A face with index > 0 of an in-memory collection | CoreText's buffer loader reads only face 0. On-disk collections (`.ttc`) load by index through `CTFontManagerCreateFontDescriptorsFromURL`. |
| A variable-font named instance | Selected by FreeType's instance index, which CoreText does not take. |

Apple Color Emoji (sbix), system TTCs (Hiragino, Apple SD Gothic Neo) and
plain outline fonts are drawn by CoreText. The T0 emoji set (1,376
codepoints) is checked against Apple Color Emoji: every codepoint it has a
glyph for must rasterize in color. Missing ones go to the fallback engine
(ft-yccm0.4.3.5).

## Expected differences from the FreeType goldens

The `real/coretext-<scene>` corpus fixtures play eight text scenes with
CoreText. Each pins its own golden and is measured against its FreeType
twin's golden (`reference_golden`). The fixture fails if that mean SSIM drops
below `reference_min_ssim`. The differences are antialiasing only:

- CoreText applies no hinting, so stems do not snap to the pixel grid as
  FreeType's light hinting does. Edge pixels differ by up to about 60/255
  (`l_inf`).
- Coverage differs slightly along glyph edges, most near curves and
  diagonals. The mean SSIM barely moves, and the worst 8x8 window falls to
  about 0.5–0.95.
- Color emoji from the bundled COLR v1 font take the same FreeType path as
  the goldens, so the emoji scene differs only as much as the text scenes
  do.

Measured with the CoreText build against the FreeType goldens:

| Scene | Mean SSIM | Worst window | l_inf | Floor |
| --- | --- | --- | --- | --- |
| `coretext-ascii-size-13` | 0.9998 | 0.849 | 61 | 0.998 |
| `coretext-ascii-size-18` | 0.9998 | 0.638 | 56 | 0.998 |
| `coretext-attributes` | 0.9997 | 0.537 | 62 | 0.998 |
| `coretext-box-drawing-powerline` | 0.9999 | 0.937 | 66 | 0.998 |
| `coretext-cjk-wide` | 0.9996 | 0.661 | 61 | 0.998 |
| `coretext-combining-marks` | 0.9997 | 0.661 | 61 | 0.998 |
| `coretext-emoji-wide-vs16-zwj` | 0.9997 | 0.803 | 61 | 0.998 |
| `coretext-ligatures` | 0.9999 | 0.795 | 51 | 0.998 |

The floor is the documented tolerance. It sits below every measured value,
leaving room for antialiasing drift, and far above a broken rasterizer: a
scene whose color emoji all came out blank measured 0.955.

## Cost

`cargo bench -p frankenterm-font --bench rasterize_glyph` measures the cost
per glyph of FreeType and CoreText. The inputs are the built-in JetBrains
Mono's printable ASCII and, on macOS, Apple Color Emoji's Emoticons block, at
13 pt and 144 dpi.
