# Vendored C sources

Built statically by `build.rs` for the FreeType rasterizer.

| Directory | Upstream | Version | Notes |
|-----------|----------|---------|-------|
| `libpng/` | https://github.com/pnggroup/libpng, tag `v1.6.59` (tag archive sha256 `2540302a1844ad2b2b501977abecfa850f265f97b78f065a712ab4074a89f5b5`) | 1.6.59 | Unmodified except: `contrib/` and `projects/` are not imported, and `.gitignore` is FrankenTerm's. `build.rs` compiles the core `png*.c` files, the `arm/` NEON files on aarch64/arm, and copies `scripts/pnglibconf.h.prebuilt`. Updated from 1.6.44 on 2026-10-13 for CVE-2026-46675 (read-after-free after malformed compressed zTXt/iTXt/iCCP chunks, fixed in 1.6.59), which FreeType's PNG glyph path (`sfnt/pngshim.c`) can reach. |
| `zlib/` | https://github.com/madler/zlib, tag `v1.3.2` (tag archive sha256 `b99a0b86c0ba9360ec7e78c4f1e43b1cbdf1e6936c8fa0f6835c0cd694a495a1`) | 1.3.2 | Unmodified except: `doc/` is not imported. Upstream's own `.gitignore` lists `zconf.h` and `Makefile` although the release ships both; they are force-added (`git add -f`), and `zconf.h` is required by every build. Updated from 1.3.1 on 2026-10-13 (hardening release: negative-length checks in crc32_combine, initialized-only copies in inflateCopy/deflateCopy, zeroed inflate state, NULL-input checks). |
| `freetype2/` | https://github.com/freetype/freetype (mirror of gitlab.freedesktop.org/freetype), tag `VER-2-14-3` (tag archive sha256 `dc49de6b01a266eef4876a4dd34d9842c475d3e28ff2eff63bd2fb760ab56261`) | 2.14.3 | Unmodified except: `docs/`, `objs/`, `tests/` and `subprojects/dlg` are not imported, and `.gitignore` is FrankenTerm's. FrankenTerm's configuration is in `build.rs` (five `ftoption.h` options, each required to match exactly once). Updated from 2.13.3 on 2026-10-13; the vendored 2.13.3 matched its tag exactly under this policy. Bindings in `src/` were checked by generating them from both versions' headers with bindgen 0.73.2: the only change in used types is `FT_Span.x` (`c_short` to `c_ushort`, same layout), edited by hand. |

To update FreeType: replace `freetype2/` with the new tag's git tree minus the directories above, keep the `.gitignore`, generate bindings from the old and new headers with the same bindgen (flags in `regenerate.sh`) and carry any layout changes into `src/`, then run the GPU snapshot harness with `FT_GPU_HARNESS_REAL_RENDERER=1`.

To update zlib: replace `zlib/` with the new tag's tree minus `doc/`, then compare `find zlib -type f` with `git ls-files zlib` and force-add anything upstream's `.gitignore` hides.

To update libpng: replace `libpng/` with the new tag's tree minus `contrib/` and `projects/`, keep this `.gitignore`, check that every file `build.rs` names still exists, and run the GPU snapshot harness with `FT_GPU_HARNESS_REAL_RENDERER=1` (the `real/emoji-*` scenes decode PNG color glyphs through it).
