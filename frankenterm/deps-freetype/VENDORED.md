# Vendored C sources

Built statically by `build.rs` for the FreeType rasterizer.

| Directory | Upstream | Version | Notes |
|-----------|----------|---------|-------|
| `libpng/` | https://github.com/pnggroup/libpng, tag `v1.6.59` (tag archive sha256 `2540302a1844ad2b2b501977abecfa850f265f97b78f065a712ab4074a89f5b5`) | 1.6.59 | Unmodified except: `contrib/` and `projects/` are not imported, and `.gitignore` is FrankenTerm's. `build.rs` compiles the core `png*.c` files, the `arm/` NEON files on aarch64/arm, and copies `scripts/pnglibconf.h.prebuilt`. Updated from 1.6.44 on 2026-10-13 for CVE-2026-46675 (read-after-free after malformed compressed zTXt/iTXt/iCCP chunks, fixed in 1.6.59), which FreeType's PNG glyph path (`sfnt/pngshim.c`) can reach. |
| `zlib/` | https://zlib.net | 1.3.1 (per `zlib/zlib.h`) | |
| `freetype2/` | https://freetype.org | 2.13.3 (per `freetype2/include/freetype/freetype.h`) | #120 reports FrankenTerm changes here (subpixel rendering); diff against upstream 2.13.3 before updating. |

To update libpng: replace `libpng/` with the new tag's tree minus `contrib/` and `projects/`, keep this `.gitignore`, check that every file `build.rs` names still exists, and run the GPU snapshot harness with `FT_GPU_HARNESS_REAL_RENDERER=1` (the `real/emoji-*` scenes decode PNG color glyphs through it).
