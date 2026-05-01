# Color-Management Audit

**Bead:** [BR-TERM-EMULATOR-UPLIFT.A11Y.3] / `ft-mpc9b.10.3`
**Scope:** What does the renderer do today with ICC profiles,
Display-P3, Rec.2020, and the 24-bit color cube? Where are the gaps,
and how does the regression fixture in
`crates/frankenterm-core/tests/color_regression_fixture.rs` close
them?

A renderer that drops or skips per-display ICC profile application
produces wrong colors silently. The visual-regression lane in
`ft-mpc9b.1.6` cannot catch this — it runs SSIM/ΔL∞ on the
render-target's *native* color space, so profile-space drift looks
identical at the pixel comparator. Only a **perceptually-aware
metric** (CIE ΔE2000 against an authored reference) catches it.

## Headline finding

| Concern                                  | Today                                        | Gap                                        |
| ---------------------------------------- | -------------------------------------------- | ------------------------------------------ |
| Surface format selection                 | `select_surface_format()` (`webgpu.rs:362`) prefers the **sRGB** suffix variant of whatever wgpu offers first. | No wide-gamut preference; no per-display ICC probe. |
| Atlas texture format                     | Hardcoded `Rgba8UnormSrgb` (`webgpu.rs:218`).| Implicit sRGB everywhere; P3 content from images / themes is silently downsampled. |
| Shader output color space                | The fragment shader writes premultiplied RGBA in linear-light into an sRGB-encoded surface, relying on wgpu's `*UnormSrgb` automatic encoding. | No declaration that the output matches any specific color space; P3 displays apply a default mapping. |
| ICC profile detection per-display        | None. No `ColorSync` (macOS), `colord` / `mutter`-portal (Linux), or `MPCC` (Windows) probe anywhere in the workspace. | All three platforms are unwired. |
| Wide-gamut formats (`Rgba16Float`, `Rgb10a2Unorm`, …) | `select_surface_format` would happily pick them if they came first in `caps.formats`, but the resulting render output is treated as if it were sRGB. | A correctly-color-managed renderer needs an explicit gamut declaration on the surface format and a tone-mapping path for sRGB content. |
| 24-bit color cube                        | Renders correctly on sRGB displays (the implicit assumption). | On Display-P3 displays, sRGB-authored content is over-saturated because the platform compositor's default sRGB → P3 transform is applied to content that is already authored for sRGB. |

**Bottom line.** Today the renderer does correct sRGB and *only* sRGB.
A Display-P3 monitor showing P3-authored content (image previews,
theme colors with P3 specifications) gets one of two wrong outputs —
silently — depending on whether the surface format ends up sRGB or
wide-gamut.

## Code citations

### `crates/frankenterm-gui/src/termwindow/webgpu.rs`

- `select_surface_format` (line 362). Picks the first format from
  `caps.formats`, then upgrades to that format's sRGB-suffix variant
  if available. No awareness of `Rgba16Float` / `Rgb10a2Unorm` or
  per-display ICC.
- Texture atlas at line 218: `wgpu::TextureFormat::Rgba8UnormSrgb`,
  hardcoded.
- BGRA8 fallback at line 832-861: tests pin the current behavior; the
  fallback path inherits the same gap.

### `crates/frankenterm-gui/src/shader.wgsl`

- The shader emits `vec4<f32>` in linear-light; wgpu's `*UnormSrgb`
  surface format performs the linear → sRGB encoding on write. No
  declaration of the *intended* output color space anywhere in the
  shader text.

### `crates/frankenterm-gui/src/glyphcache.rs`

- Image data is decoded as `Rgba8` / `AnimRgba8` and uploaded to the
  atlas as-is. There is no path that records the source image's ICC
  profile or color space; the renderer assumes every image is sRGB.

### Per-platform window code

No ICC-related code in `frankenterm/window/src/os/{macos,wayland,x11}/`.
The macOS path could call `CGColorSpaceCreateWithICCProfile` /
`CGDisplayCopyColorSpace` against `[NSScreen mainScreen].colorSpace`;
the Linux paths could query `org.freedesktop.portal.Settings` for the
display profile or use `colord-rs`. None of this is wired today.

## Initial fix shipped with this bead

This bead ships the **observability and regression-prevention
foundation**, not the per-display ICC integration:

1. **Core color-management module** at
   `crates/frankenterm-core/src/color_management.rs`:
   - `ColorSpace` enum (sRGB, Display-P3, Rec.2020, Rec.709,
     ProPhoto-RGB, Unknown).
   - `Color` (RGBA8 tagged with source space).
   - `IccProfile` metadata struct.
   - `ColorPattern` (24-bit cube, P3 gamut probes, Rec.2020 probes,
     gamma ramp).
   - CIE ΔE2000 implementation per Sharma 2005 with reference-data
     unit tests (red↔blue, white↔black, identity, symmetry).
   - `SurfaceFormatGamut` classifier mapping wgpu format names to
     `ColorSpace` + `wide_gamut_unverified` / `hdr_capable` flags.
   - `ColorMeasurement` (one row of the JSONL log).

2. **Surface gamut diagnostic** in `webgpu.rs`:
   - `classify_surface_color_space()` calls into `SurfaceFormatGamut`.
   - Surface configuration logs the chosen format's color space and
     `wide_gamut_unverified` flag at startup (`log::info!`).
   - 3 new unit tests pin the classifier's bridge to wgpu's `Debug`
     names.

3. **Regression fixture** at
   `crates/frankenterm-core/tests/color_regression_fixture.rs`:
   - Per-pattern golden snapshot tests with `FT_COLOR_BLESS=1`
     deliberate-bless flow.
   - 4 proptest properties on ΔE2000 (non-negative, symmetric,
     identity, measurement-ctor totality) over 256 cases each.
   - Sentinel tests
     (`wide_gamut_surface_formats_are_marked_unverified`,
     `srgb_surface_formats_are_marked_verified`) that fire when an
     integration lands without flipping the corresponding flag.

## Closure plan

The follow-on integration work that fills the gap above lives in
sibling beads under `ft-mpc9b.10`:

- **macOS ColorSync integration** (one bead): `CGDisplayCopyColorSpace`
  against `[NSScreen mainScreen]`; populate `IccProfile` with the
  display's gamut; flip
  `SurfaceGamutClassification::wide_gamut_unverified` to `false` and
  add the verified gamut.
- **Linux ICC integration** (one bead): query
  `org.freedesktop.portal.Settings` for the active display profile;
  fall back to the X11 `_ICC_PROFILE` window property.
- **Wide-gamut surface preference** (one bead): when the platform
  reports a wide-gamut profile and wgpu offers a `Rgba16Float` or
  `Rgb10a2Unorm` surface format, prefer it over the sRGB-suffix
  variant; route sRGB content through a tone-mapping shader pass.
- **CI calibrated lane** (one bead): a color-managed test machine
  produces goldens at `tests/color/golden/<platform>-<scenario>.jsonl`
  alongside the synthetic baselines committed by this bead.

## Per-release attestation

Per the bead's acceptance criterion, the
`crates/frankenterm-core/src/release_attestation.rs` schema gains a
`color_management_coverage` field on the next release-attestation
schema bump. Until then, the regression fixture's `cargo test
--package frankenterm-core --test color_regression_fixture` invocation
is the gating signal.
