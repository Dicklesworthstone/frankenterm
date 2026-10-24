# cairo-sys-rs, built from vendored cairo and pixman sources

FrankenTerm's font crate paints color glyphs and outline paths with cairo. The
published `cairo-sys-rs` links the system cairo through pkg-config, which on
macOS means Homebrew's `libcairo.2.dylib` and its closure (fontconfig, freetype,
libpng, gettext, pixman, and the X11/xcb libraries). `create-macos-bundle.sh`
then has to copy those 14 libraries into the app and raise
`LSMinimumSystemVersion` to the highest minimum any of them declares. Homebrew
bottles are built for the host's macOS, so a bundle built on a macOS 27 host
could only be opened on macOS 27, although every FrankenTerm executable is
built for macOS 11.0.

This package overrides the registry dependency (`[patch.crates-io]` in the
workspace manifest) with a static build of the minimal cairo that FrankenTerm
uses: the image, recording, tee, observer and user-font surfaces, with no
fontconfig, freetype, png, PDF/PS/SVG or X11 backends. The GUI then has no
non-system library dependencies, and the bundle's native closure is empty.

## Provenance

- `cairo/`, `pixman/`, `build.rs`, `LICENSE`: upstream WezTerm's `deps/cairo`
  at commit `0f180b000` (2025-03-06), unchanged except `build.rs` as noted
  below. That tree holds cairo 1.17.8 and pixman 0.42.2 as imported by
  WezTerm's `import-cairo.sh` and `import-pixman.sh`. Cairo's file list in
  `build.rs` comes from a meson static build with the optional backends off.
- `src/lib.rs`: the crates.io `cairo-sys-rs` 0.22.0 bindings, which
  `cairo-rs` 0.22 requires (WezTerm's copy targets 0.18).

## Local changes

- `Cargo.toml`: version 0.22.0 with the published crate's feature names. The
  backend features (`png`, `pdf`, `svg`, `ps`, `xlib`, `xcb`, `freetype`,
  `script`, `use_glib`, `quartz-surface`, `win32-surface`) are accepted but add
  nothing; the workspace enables none of them (`default-features = false`).
  Declared license: `MIT AND (LGPL-2.1-only OR MPL-1.1)` (the bindings and
  pixman are MIT; cairo is LGPL-2.1 or MPL-1.1, and `deny.toml` allows
  LGPL-2.1-only). The license texts are `LICENSE`, `cairo/COPYING*` and
  `pixman/COPYING`.
- `src/lib.rs`: the `use_glib` module (`gobject.rs`) is removed, since this
  package has no `glib-sys` dependency.
- `build.rs`: on non-Windows targets cairo is built with pthread mutexes and
  compiler atomics (`CAIRO_HAS_PTHREAD`, `HAVE_CXX11_ATOMIC_PRIMITIVES`) and
  pixman with thread-local caches (`TLS=__thread`). WezTerm builds both without
  thread safety (`CAIRO_NO_MUTEX`, `PIXMAN_NO_TLS`) because it rasterizes on one
  thread; FrankenTerm's Metal front end rasterizes on a render thread per
  window, and the system libraries this replaces are thread-safe. Windows keeps
  WezTerm's settings; the font crate depends on cairo only on non-Windows
  targets, so nothing builds it there.

## Verification (2026-10-13)

On aarch64-apple-darwin (macOS 27):

- `frankenterm-gui` links no non-system library (`otool -L`), defines cairo's
  symbols itself, and keeps `minos 11.0`.
- `cargo test -p frankenterm-font --lib`: the same results as with the system
  cairo.
- The real-renderer golden scenes that reach cairo through the FreeType
  rasterizer are pixel-identical to their goldens (0 changed pixels):
  `real/emoji-wide-vs16-zwj`, `real/box-drawing-powerline`, `real/cjk-wide`,
  `real/combining-marks`, `real/attributes`, `real/ligatures`,
  `real/background-gradient`
  (`FT_GPU_HARNESS_REAL_RENDERER` path of `tests/gpu_regression.rs`).

On x86_64-unknown-linux-gnu (Ubuntu, without relying on `libcairo2-dev`):

- `cargo test -p frankenterm-font --lib` builds and passes (50 tests); the test
  binary defines cairo's and pixman's symbols and loads neither `libcairo` nor
  `libpixman` (`ldd`).
