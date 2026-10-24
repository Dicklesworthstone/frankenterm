# ft-yccm0.7 defect 2 / ft-34at5: windows behind another FrankenTerm window

Native, mac-mini-old (macOS 26.5, 1683x668 pt at 2x), 2026-10-14.

## Why the operator's back windows never parked

The operator's GUI (d79bd79, 11 stacked full-screen windows, smart glass) kept
33 drawables (2.6 GB) live: no window was ever reported occluded. The
`occlusion_probe.swift` app (accessory, never activates) orders small windows
over one another and reads `occlusionState` of the bottom one ("probe").
All five runs are in `occlusion-probe-results.txt`:

- A **titled** cover never hides the probe: not a stock `NSWindow` 20 pt
  larger on every side, not one with a solid titlebar or a black background,
  not one reaching 60 pt above the probe. CGWindowList confirms the cover is
  in front. The operator's `RESIZE` decorations are titled.
- An **untitled** cover hides the probe only when its **layer** is opaque.
  `NSWindow.opaque` with a non-opaque layer leaves the probe visible. An
  opaque layer without `NSWindow.opaque` hides it.

So stacked FrankenTerm windows are never occluded for AppKit, and ft-34at5's
parking, which acts on AppKit occlusion, never runs on that setup.

## Change (1a11ac1f2)

The macOS window layer marks a window covered when another of the app's
windows meets all of these:
- it is in front of the window, visible and on the active Space;
- its frame contains the window's frame;
- it draws only opaque pixels (`Config::window_content_is_opaque`).

A titled cover hides only titled windows. Covered counts as occluded, so the
GUI parks the surface after 5 s and unparks and repaints it when uncovered.

## A/B (`cover_park.py`)

The setup is one GUI with three windows, each 210x124 pt, all printing
continuously. They are stacked at the same spot and brought in front of the
other apps' windows, with no keyboard focus taken. The windows are opaque:
opacity 1.0 with an opaque gradient layer, `RESIZE|MACOS_FORCE_DISABLE_SHADOW`,
`max_fps` 60.

The GUI's own timers then:
- move the front window 200 pt right at 22 s;
- turn the middle window glass (opacity 0.9) at 42 s.

The "presented" column is frames per second for the whole GUI.

| phase | installed d79bd79 | 1a11ac1 |
|---|---|---|
| three stacked (12-21 s) | 183.0 fps, drawables 3.6 MiB | 57.6 fps, 1.2 MiB: two covered windows parked |
| front moved aside (32-41 s) | 172.8 fps, 3.6 MiB | 121.7 fps, 2.4 MiB: back window still covered |
| middle turned glass (52-61 s) | 182.7 fps, 3.6 MiB | 183.3 fps, 3.6 MiB: nothing covered, all three paint |

In each 1a11ac1 phase, only the windows a viewer can see present frames. A
window that is uncovered paints again. The debug log's cover transitions are in
`run-1a11ac1.txt`; the startup flaps come from the three `focus()` calls that
reorder the stack.

`winmap.swift` prints, for each window, the windows in front of it that
intersect it (CGWindowList, no Screen Recording permission). An earlier pair of
runs was invalid: a full-width Ghostty window was in front of windows 1 and 2,
so AppKit occluded them and wgpu refused their drawables in both arms. The
harness now brings its windows forward first. `winmap-run.sh` maps the windows
during a run.

## Operator setup

The smart-glass Lua makes back windows opaque (opacity 1.0, blur 0). They also
need an opaque bottom layer to count as covers: the gradient at opacity 1.0
instead of 0.92. With that, the windows behind the one under the glass front
window stop painting and release their swapchains, about 2.1 GB at 6144x3200.
