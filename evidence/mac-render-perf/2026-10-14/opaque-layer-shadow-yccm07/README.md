# ft-yccm0.7 defect 1: an opaque WebGPU layer does not make the window shadow cheaper (native)

**Question.** ft-yccm0.7 measured 11 stacked opaque windows at 67.4% of the GPU in WindowServer, and
9.6% with `MACOS_FORCE_DISABLE_SHADOW`. The hypothesis was that the WebGPU surface (always
`PostMultiplied`) left the `CAMetalLayer` non-opaque, so WindowServer rebuilt each shadowed window's
shadow from the layer's alpha on every frame. 95dc36174 configures the surface `Opaque` for opaque
windows (wgpu-hal's Metal backend then sets `CAMetalLayer.opaque`).

**Setup** (`ws_gpu_ab.py`): mac-mini-old (arm64 Mac mini, macOS 26.5, 1920x1080 at 60 Hz, load ~2.5,
no Screen Sharing viewer). Three overlapping 150x40 windows, default look (opacity 1.0, window
shadow on, `front_end = "WebGpu"`, `max_fps = 60`), each printing ~250 lines/s, so each presents at
~56 fps. Every GUI in a throwaway HOME, launched with `open -g -n`, stopped by exact pid. Arms in
rotation, 3 cycles: `old` = installed b088a4e, `new` = 95dc36174, `new-noshadow` = 95dc36174 with
`window_decorations = 'TITLE|RESIZE|MACOS_FORCE_DISABLE_SHADOW'`. Per block: 8 s settle, then 15 s of
WindowServer's and the GUI's `accumulatedGPUTime` (ioreg `IOGPUDeviceUserClient`) and the GUI's own
`frames.presented_total`. The `new` arms logged `alpha_mode=Opaque`.

**Result** (`ab-mini-old.jsonl`; WindowServer share of the GPU):

| arm | block 1 | block 2 | block 3 |
|-----|---------|---------|---------|
| old | invalid (-215.5: a WindowServer GPU client reset inside the block) | 29.5% | 28.8% |
| new (opaque layer) | 28.7% | 32.0% | 28.7% |
| new, shadow disabled | 6.3% | 6.1% | 6.1% |

The GUI presented 165-169 fps in total in every block, at 11.8-14.3% of the GPU.

**Reading.** The layer's opacity makes no difference: the shadow costs about 23 points of the GPU
for three shadowed windows presenting continuously, with an opaque or a non-opaque layer. The
hypothesis is refuted. 95dc36174 stays: an opaque window now presents through an opaque layer, as the
Metal front end already does. Its comment in `webgpu.rs` was corrected to these numbers. Defect 1's
acceptance criterion (default decorations within 2 points of the shadow-disabled figure) is not met
by it.
