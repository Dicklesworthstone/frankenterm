# Linux GPU CI feasibility for the GPU regression harness

**Bead:** ft-ombfl.16  
**Date:** 2026-04-28  
**Status:** Spike complete; do not hard-gate Linux yet.

## Question

Can the headless GPU regression harness run on Linux CI through Mesa software
rasterizers (`llvmpipe` for OpenGL/Gallium and `lavapipe`/swrast for Vulkan),
and should it share the macOS Metal golden set?

## Current harness surface

The checked-in harness path is `crates/frankenterm-gui/tests/gpu_regression.rs`
with the feature-gated `frankenterm_gui::headless_render` entrypoint.

The renderer:

- creates a `wgpu::Instance` with `Backends::all()`;
- requests a high-performance adapter with `force_fallback_adapter: false`;
- renders into an offscreen `Rgba8UnormSrgb` texture;
- reads back tightly packed RGBA8;
- emits JSON-line `gpu` metadata: backend, adapter name, device type, driver,
  and driver info;
- has a `--headless-render-self-test` path that renders the same 64x64
  fixture 10 times and asserts byte-identical RGBA output.

The fixture tree currently contains one renderer-free `_smoketest` fixture
under `tests/golden/gpu`. That fixture validates PNG decode, metric
calculation, and artifact behavior, but it cannot answer Linux GPU
determinism or cross-platform golden reuse by itself.

## External runner facts

- GitHub documents standard hosted runner pricing at USD 0.006/min for Linux
  2-core and USD 0.062/min for macOS 3-core or 4-core runners. Linux 4-core GPU
  larger runners are listed separately at USD 0.052/min and are not part of the
  standard `ubuntu-latest` path. Source:
  <https://docs.github.com/en/billing/reference/actions-runner-pricing>
- GitHub hosted runners are fresh VMs, standard images are maintained in
  `actions/runner-images`, and GPU-powered machines are larger runners
  available to Team/Enterprise Cloud organizations. Source:
  <https://docs.github.com/en/actions/concepts/runners/github-hosted-runners>
- The current `actions/runner-images` table maps `ubuntu-latest` to Ubuntu
  24.04 x64 and maps `macos-15` to macOS 15 Arm64. Source:
  <https://github.com/actions/runner-images>
- Mesa documents `llvmpipe` as a multithreaded software rasterizer using LLVM
  JIT code generation. That is suitable for a deterministic software-renderer
  experiment, but it is not a literal match for Apple Metal. Source:
  <https://docs.mesa3d.org/drivers/llvmpipe.html>
- The Ubuntu 24.04 runner image README lists `xvfb`, browser stacks, and common
  build packages, but it does not establish a pinned Mesa Vulkan software
  adapter contract for this harness. Source:
  <https://raw.githubusercontent.com/actions/runner-images/main/images/ubuntu/Ubuntu2404-Readme.md>

## Probe commands

All Cargo probes use the required isolated target dir:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-cod_3-target \
  cargo test -p frankenterm-gui --features headless-render \
  --test gpu_regression -- --headless-render-self-test --nocapture
```

The corresponding fixture-only scaffold command is:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/ft-cod_3-target \
  cargo test -p frankenterm-gui --test gpu_regression -- --self-test --nocapture
```

## Determinism evidence

| Signal | Runner/backend | Fixture count | Iterations | SSIM | L-inf | Changed fraction | Result |
|---|---:|---:|---:|---:|---:|---:|---|
| Static comparator self-test | CPU-only harness code | synthetic | 1 | 1.0 for identical case | 0 for identical case | 0.0 for identical case | Validates metric behavior, not GPU |
| Headless renderer self-test | Darwin 25.2.0, Apple M4 Pro, Metal, IntegratedGpu | 1 synthetic 64x64 render | 10 | 1.0 by byte equality | 0 by byte equality | 0.0 by byte equality | Pass; render iterations 80, 15, 19, 16, 17, 19, 16, 19, 18, 19 ms |
| Cross-platform macOS Metal vs Linux Mesa | Not available in this checkout | 0 real renderer fixtures | 0 | n/a | n/a | n/a | Needs dedicated fixture capture job |

The current harness can prove same-adapter repeatability once a usable adapter
initializes. It cannot yet prove macOS Metal and Linux Mesa can share one
golden set because there are no real headless terminal fixtures and no
cross-runner artifact pair to compare.

The required `rch exec` probe executed on the local Darwin host rather than a
Linux worker for this non-standard test invocation (`rch exec -- uname -a`
reported `Darwin Mac-mini-max 25.2.0 ... arm64`). That makes the Metal
repeatability result concrete, but leaves Linux llvmpipe/lavapipe unmeasured.

## Linux CI assessment

Linux via Mesa is feasible as a **soft diagnostic lane**, but not as a hard
merge gate yet.

Required changes before a real Linux lane:

1. Add an explicit software-renderer mode to the harness. The current
   `force_fallback_adapter: false` and high-performance adapter request do not
   deliberately select lavapipe/llvmpipe.
2. Log the selected adapter as part of the required JSONL row, then reject the
   run unless it matches the expected software backend. A minimal row should be:

   ```json
   {"phase":"render-frame","runner":"ubuntu-24.04","backend":"Vulkan","adapter":"llvmpipe (LLVM ... )","driver":"lavapipe","fixture":"cursor-basic","ssim":1.0,"l_inf":0,"changed_pixel_fraction":0.0}
   ```

3. Install or validate Mesa software Vulkan packages in CI before running the
   harness. The GitHub runner image docs do not provide a stable project-level
   guarantee that the needed adapter is already installed and selected.
4. Capture at least five real renderer fixtures on both `macos-15` and the
   pinned Linux software adapter: cursor, selection, scrollback, box drawing,
   and emoji/color fallback.
5. Compare Linux actuals against macOS goldens and record per-fixture SSIM,
   L-inf, and changed-pixel-fraction. Only then decide whether to share goldens.

## Cost and benefit

| Option | CI cost | Determinism | Coverage value | Recommendation |
|---|---:|---|---|---|
| macOS Metal hard gate only | High: USD 0.062/min standard macOS | Best match for current reference platform | Catches production regressions on the platform used for goldens | Keep as hard gate |
| `ubuntu-latest` Mesa soft lane | Low: USD 0.006/min standard Linux plus setup time | Unknown until adapter is pinned and measured | Cheap early signal for cross-platform render drift | Add after software adapter selection exists |
| GitHub Linux GPU larger runner | Medium: USD 0.052/min | Hardware/driver-specific, not software deterministic | Useful for native Linux GPU bugs, not for stable goldens | Do not use for goldens yet |
| Separate Linux golden set | Low runtime cost, higher review/storage cost | Strong if adapter is pinned | Lets Linux soft lane become meaningful without forcing Metal parity | Likely needed if any of five fixtures exceed thresholds |

## Recommendation

Do not share one golden set or hard-gate Linux in the next CI change.

Proceed with:

- `macos-15` Metal as the hard-gated reference lane;
- `ubuntu-24.04`/`ubuntu-latest` Mesa as a non-blocking pilot only after the
  harness can intentionally select and assert a software adapter;
- a separate Linux golden namespace if the five-fixture pilot produces any
  fixture with `ssim < 0.99`, `l_inf > 8`, or
  `changed_pixel_fraction > 0.001` against the macOS golden.

This is a no-go for Linux hard gating today and a go for a follow-up pilot.

Follow-up filed: `ft-ombfl.17` ("Pilot Linux llvmpipe GPU harness lane").

## ft-ombfl.17 pilot wiring

The initial pilot is the `GPU Linux llvmpipe Pilot` job in
`.github/workflows/ci.yml`. It runs only five representative fixtures to keep
Linux CI cost bounded:

- `text-basic-paragraph`
- `text-box-drawing`
- `cursor-block-steady`
- `selection-word`
- `overlay-visual-mode`

The job installs Mesa's Vulkan software stack plus the X11/X11-XCB, XCB-util,
XCB-image, xkbcommon, xkbcommon-X11, and Cairo pkg-config/link development
stubs needed by the GUI dependency graph on `ubuntu-24.04`, forces the
headless renderer through `WGPU_BACKEND=vulkan`,
`LIBGL_ALWAYS_SOFTWARE=1`, `GALLIUM_DRIVER=llvmpipe`, and
`VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`, and forces rustix's
stable libc backend for the transitive `k9`/`terminal_size` dev-dependency path
while current nightly rejects rustix 0.37's auto-detected internal
`rustc_attrs` linux_raw path. It then asserts adapter metadata contains
`llvmpipe`.

It writes Linux captures and `generated_at_runner=ubuntu-24.04-llvmpipe`
metadata to `target/gpu-regression/linux-llvmpipe-goldens/` instead of
mutating the checked-in macOS reference fixtures. The job then validates that
namespace against itself as the determinism gate. A final
macOS-vs-Linux comparison runs against `tests/golden/gpu`; exit code `1` is
converted to a GitHub warning and uploaded as
`target/gpu-regression/linux-vs-macos/` artifacts so platform divergence is
visible without making the pilot a hard cross-platform golden gate.
