//! Native Metal renderer for the FrankenTerm GUI on macOS.
//!
//! This crate is the Track C foundation of the mac-render campaign (bead
//! `ft-yccm0.4.1.1`). The GUI owns the render state machine; this crate owns
//! the Metal objects: the [`MetalDevice`] (device, command queue, capability
//! probe) and the [`MetalRenderer`] bound to a window's `CAMetalLayer`.
//!
//! What it renders today is deliberately minimal: one render pass that clears
//! the drawable to the window background color and presents it. That proves
//! the device, queue, layer and present plumbing end to end. Cell backgrounds,
//! glyphs and the GPU data model arrive with the later Track C beads, so
//! `front_end = "Metal"` is an in-development opt-in that does not yet draw
//! terminal content.
//!
//! On every target other than macOS the same API exists as an uninhabited
//! stub: [`MetalDevice::system_default`] and [`MetalRenderer::attach`] return
//! [`MetalUnavailable::UnsupportedPlatform`], so callers need no `cfg` and the
//! GUI falls back to WebGpu.
//!
//! # Fallback contract
//!
//! The GUI requests Metal and keeps it only when [`MetalRenderer::attach`]
//! succeeds. Every refusal is a [`MetalUnavailable`] with a stable
//! [`MetalUnavailable::code`] for logs and a human-readable `Display`. A
//! device is admitted only if it supports `MTLGPUFamilyApple7` or newer
//! ([`MIN_APPLE_FAMILY`], the M1 generation): the renderer is designed for
//! Apple-silicon unified memory and tile-based deferred rendering, so Intel
//! and discrete-GPU Macs fall back to WebGpu.
//!
//! # UNSAFE-CONTRACT
//!
//! FrankenTerm forbids `unsafe` workspace-wide. This crate is an audited
//! native/FFI exception (AGENTS.md "Unsafe code"): `unsafe_code` is denied
//! crate-wide, every block opts in with a narrow `#[allow(unsafe_code)]`, and
//! clippy's `undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block`
//! are denied, so each block holds one operation and one `SAFETY:` comment
//! naming its category below. All blocks live in `src/macos.rs`. The non-macOS
//! stub contains none.
//!
//! Most Metal and Core Animation calls are safe in `objc2-metal` /
//! `objc2-quartz-core` 0.3, whose generated bindings already encode the
//! Objective-C signatures and retain/release rules. The remaining unsafe
//! operations fall into four categories:
//!
//! 1. **FFI-VIEW: borrowing the AppKit view.** `MetalRenderer::attach`
//!    dereferences the `NSView` pointer from a borrowed
//!    `raw_window_handle::WindowHandle` and sends it `-layer`.
//!    *Invariant:* the borrowed `WindowHandle<'_>` guarantees the view is alive
//!    for the duration of the call (raw-window-handle 0.6 contract); the
//!    pointer is only dereferenced on the main thread (checked with
//!    `MainThreadMarker` first, as AppKit requires); the message is declared
//!    with its exact Objective-C type (`-[NSView layer]` returns a nullable
//!    `CALayer *`); and the returned layer is retained, so the renderer never
//!    depends on the view's lifetime afterwards. The layer is accepted only if
//!    a checked downcast proves it is a `CAMetalLayer`.
//! 2. **FFI-INDEX: render-pass attachment indexing.**
//!    `colorAttachments[0]` is fetched with `objectAtIndexedSubscript`.
//!    *Invariant:* index 0 is always below the eight color attachments every
//!    Metal device exposes, and the descriptor array creates the attachment
//!    descriptor on first access, so the returned object is valid.
//! 3. **FFI-EXTENT: texture descriptors and blit regions.** Creating a 2D
//!    texture descriptor and copying a texture region into a buffer.
//!    *Invariant:* width and height are checked to be in `1..=MAX_TEXTURE_EXTENT`
//!    (the Apple-family 2D texture limit) before the call; the blit copies
//!    exactly the texture's own `width x height x 1` region of slice 0, level 0;
//!    the destination row pitch is `width * 4` bytes for the 4-byte BGRA8
//!    format, and the destination buffer is allocated with exactly
//!    `row pitch * height` bytes, so the copy stays in bounds on both sides.
//! 4. **BUFFER-CONTENTS: reading shared GPU memory.** Reading back the bytes
//!    of a `MTLStorageModeShared` buffer through `-contents`.
//!    *Invariant:* the pointer covers `-length` bytes, which is at least the
//!    requested length (checked); the command buffer that wrote it has been
//!    waited on to completion and checked for success, so no GPU write races
//!    the read; the bytes are copied into an owned `Vec` before the buffer is
//!    released, so no reference outlives the allocation.
//!
//! Thread affinity: `MetalRenderer` holds Objective-C objects that are not
//! `Send`, so it stays on the thread that created it (the GUI main thread
//! today; bead `ft-yccm0.4.1.2` moves rendering to a dedicated thread).

use std::fmt;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{MetalDevice, MetalRenderer};

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::{MetalDevice, MetalRenderer};

/// Lowest `MTLGPUFamilyAppleN` the renderer admits: Apple7, the M1 generation
/// and the first Apple-silicon Mac family.
pub const MIN_APPLE_FAMILY: u8 = 7;

/// Highest `MTLGPUFamilyAppleN` the pinned `objc2-metal` 0.3.2 bindings name.
pub const MAX_KNOWN_APPLE_FAMILY: u8 = 10;

/// Largest texture width or height accepted for offscreen rendering: the 2D
/// texture limit of every Apple3-or-newer GPU family.
pub const MAX_TEXTURE_EXTENT: u32 = 16_384;

/// A Metal GPU family the capability probe asks about.
///
/// The raw values are Apple's `MTLGPUFamily` enumerators, so a probe can be
/// answered by `-[MTLDevice supportsFamily:]` or by a test table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuFamily {
    /// `MTLGPUFamilyAppleN` for N in `1..=MAX_KNOWN_APPLE_FAMILY`.
    Apple(u8),
    /// `MTLGPUFamilyMac2`, every Metal-capable Mac GPU.
    Mac2,
    /// `MTLGPUFamilyMetal3`, the Metal 3 feature set.
    Metal3,
}

impl GpuFamily {
    /// The `MTLGPUFamily` raw value.
    #[must_use]
    pub fn raw(self) -> isize {
        match self {
            Self::Apple(n) => 1000 + isize::from(n),
            Self::Mac2 => 2002,
            Self::Metal3 => 5001,
        }
    }
}

/// What the capability probe learned about a Metal device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCapabilities {
    /// The device name, e.g. "Apple M4 Pro".
    pub name: String,
    /// The highest `MTLGPUFamilyAppleN` the device supports, if any.
    pub apple_family: Option<u8>,
    /// Whether the device supports `MTLGPUFamilyMac2`.
    pub mac2: bool,
    /// Whether the device supports `MTLGPUFamilyMetal3`.
    pub metal3: bool,
    /// Whether CPU and GPU share memory (true on Apple silicon).
    pub unified_memory: bool,
}

impl DeviceCapabilities {
    /// Builds capabilities from a `supportsFamily:`-style predicate.
    ///
    /// The Apple family is the highest N in `1..=MAX_KNOWN_APPLE_FAMILY` the
    /// predicate accepts. Apple families are cumulative on real hardware, but
    /// the scan does not rely on that: it reports the highest supported one.
    pub fn from_family_probe(
        name: impl Into<String>,
        unified_memory: bool,
        supports: impl Fn(GpuFamily) -> bool,
    ) -> Self {
        let apple_family = (1..=MAX_KNOWN_APPLE_FAMILY)
            .rev()
            .find(|&n| supports(GpuFamily::Apple(n)));
        Self {
            name: name.into(),
            apple_family,
            mac2: supports(GpuFamily::Mac2),
            metal3: supports(GpuFamily::Metal3),
            unified_memory,
        }
    }

    /// Decides whether the Metal renderer may run on this device.
    pub fn admit(&self) -> Result<(), MetalUnavailable> {
        match self.apple_family {
            Some(family) if family >= MIN_APPLE_FAMILY => Ok(()),
            apple_family => Err(MetalUnavailable::UnsupportedGpuFamily {
                device: self.name.clone(),
                apple_family,
            }),
        }
    }
}

impl fmt::Display for DeviceCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        match self.apple_family {
            Some(family) => write!(f, " (Apple{family}")?,
            None => write!(f, " (no Apple family")?,
        }
        if self.metal3 {
            write!(f, ", Metal3")?;
        }
        if self.mac2 {
            write!(f, ", Mac2")?;
        }
        if self.unified_memory {
            write!(f, ", unified memory")?;
        }
        write!(f, ")")
    }
}

/// Why `front_end = "Metal"` cannot be honored. The GUI logs the reason and
/// falls back to WebGpu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetalUnavailable {
    /// The build target is not macOS.
    UnsupportedPlatform,
    /// `MTLCreateSystemDefaultDevice` returned no device.
    NoDevice,
    /// The device is older than [`MIN_APPLE_FAMILY`] or not an Apple GPU.
    UnsupportedGpuFamily {
        device: String,
        apple_family: Option<u8>,
    },
    /// The device could not create a command queue.
    NoCommandQueue { device: String },
    /// AppKit views may only be touched on the main thread.
    NotMainThread,
    /// The window handle could not be read or is not an AppKit view.
    UnsupportedWindowHandle { kind: String },
    /// The view has no backing layer.
    NoBackingLayer,
    /// The view's backing layer is not a `CAMetalLayer`.
    LayerNotMetal { class: String },
}

impl MetalUnavailable {
    /// A stable snake_case code for logs and diagnostics.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::NoDevice => "no_device",
            Self::UnsupportedGpuFamily { .. } => "unsupported_gpu_family",
            Self::NoCommandQueue { .. } => "no_command_queue",
            Self::NotMainThread => "not_main_thread",
            Self::UnsupportedWindowHandle { .. } => "unsupported_window_handle",
            Self::NoBackingLayer => "no_backing_layer",
            Self::LayerNotMetal { .. } => "layer_not_metal",
        }
    }
}

impl fmt::Display for MetalUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "the Metal renderer is only built for macOS"),
            Self::NoDevice => write!(f, "MTLCreateSystemDefaultDevice returned no device"),
            Self::UnsupportedGpuFamily {
                device,
                apple_family: Some(family),
            } => write!(
                f,
                "{device} supports MTLGPUFamilyApple{family}; the Metal renderer needs Apple{MIN_APPLE_FAMILY} or newer"
            ),
            Self::UnsupportedGpuFamily {
                device,
                apple_family: None,
            } => write!(
                f,
                "{device} is not an Apple-family GPU; the Metal renderer needs Apple{MIN_APPLE_FAMILY} or newer"
            ),
            Self::NoCommandQueue { device } => {
                write!(f, "{device} could not create a Metal command queue")
            }
            Self::NotMainThread => write!(f, "the Metal layer must be attached on the main thread"),
            Self::UnsupportedWindowHandle { kind } => {
                write!(f, "the window handle is {kind}, not an AppKit view")
            }
            Self::NoBackingLayer => write!(f, "the window's view has no backing layer"),
            Self::LayerNotMetal { class } => {
                write!(
                    f,
                    "the window's backing layer is a {class}, not a CAMetalLayer"
                )
            }
        }
    }
}

impl std::error::Error for MetalUnavailable {}

/// Why one frame could not be rendered. Unlike [`MetalUnavailable`], these
/// are per-frame failures the GUI retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// `-[CAMetalLayer nextDrawable]` returned nil (its one-second timeout
    /// expired, typically while the window is occluded or resizing).
    DrawableUnavailable,
    /// The command queue returned no command buffer.
    CommandBufferUnavailable,
    /// The command buffer returned no encoder.
    EncoderUnavailable,
    /// The device could not allocate a texture or buffer of `bytes`.
    AllocationFailed { what: &'static str, bytes: usize },
    /// A requested extent is zero or larger than [`MAX_TEXTURE_EXTENT`].
    InvalidExtent { width: u32, height: u32 },
    /// The command buffer finished in a state other than completed.
    CommandFailed { detail: String },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DrawableUnavailable => write!(f, "CAMetalLayer returned no drawable"),
            Self::CommandBufferUnavailable => write!(f, "Metal returned no command buffer"),
            Self::EncoderUnavailable => write!(f, "Metal returned no command encoder"),
            Self::AllocationFailed { what, bytes } => {
                write!(f, "Metal could not allocate a {bytes}-byte {what}")
            }
            Self::InvalidExtent { width, height } => write!(
                f,
                "extent {width}x{height} is outside 1..={MAX_TEXTURE_EXTENT} pixels per side"
            ),
            Self::CommandFailed { detail } => write!(f, "Metal command buffer failed: {detail}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// The outcome of a successfully encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// A drawable was cleared, scheduled for presentation and committed.
    Presented,
    /// The window has a zero-sized extent, so there was nothing to draw.
    ZeroSize,
}

/// A render-pass clear color, premultiplied, for a `BGRA8Unorm` target.
///
/// The components are sRGB-encoded values written to the framebuffer as-is:
/// `BGRA8Unorm` applies no transfer function, so the stored bytes match the
/// configured color exactly. Core Animation composites layers with
/// premultiplied alpha, so straight alpha is premultiplied on construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClearColor {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

impl ClearColor {
    /// Builds a clear color from straight-alpha sRGB components. Each
    /// component is clamped to `0.0..=1.0`, with NaN treated as `0.0`.
    #[must_use]
    pub fn from_srgba(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        let unit = |c: f32| {
            if c.is_nan() {
                0.0
            } else {
                f64::from(c.clamp(0.0, 1.0))
            }
        };
        let alpha = unit(alpha);
        Self {
            red: unit(red) * alpha,
            green: unit(green) * alpha,
            blue: unit(blue) * alpha,
            alpha,
        }
    }

    /// The `[B, G, R, A]` bytes a `BGRA8Unorm` clear stores, rounding each
    /// component to the nearest 8-bit value as Metal's unorm conversion does.
    #[must_use]
    // The clamp bounds the rounded value to 0.0..=255.0, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn to_bgra8(self) -> [u8; 4] {
        let byte = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        [
            byte(self.blue),
            byte(self.green),
            byte(self.red),
            byte(self.alpha),
        ]
    }
}

/// Extracts the AppKit `NSView` pointer from a raw window handle.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn appkit_view(
    handle: raw_window_handle::RawWindowHandle,
) -> Result<std::ptr::NonNull<std::ffi::c_void>, MetalUnavailable> {
    match handle {
        raw_window_handle::RawWindowHandle::AppKit(handle) => Ok(handle.ns_view),
        other => Err(MetalUnavailable::UnsupportedWindowHandle {
            kind: window_handle_kind(&other).to_string(),
        }),
    }
}

/// A short name for a raw window handle's platform, for diagnostics.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn window_handle_kind(handle: &raw_window_handle::RawWindowHandle) -> &'static str {
    use raw_window_handle::RawWindowHandle as H;
    match handle {
        H::AppKit(_) => "an AppKit view",
        H::UiKit(_) => "a UIKit view",
        H::Xlib(_) => "an Xlib window",
        H::Xcb(_) => "an XCB window",
        H::Wayland(_) => "a Wayland surface",
        H::Win32(_) => "a Win32 window",
        H::WinRt(_) => "a WinRT window",
        H::Web(_) => "a web canvas",
        _ => "an unsupported window handle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn probe(name: &str, unified: bool, families: &[GpuFamily]) -> DeviceCapabilities {
        DeviceCapabilities::from_family_probe(name, unified, |family| families.contains(&family))
    }

    fn apple_up_to(n: u8) -> Vec<GpuFamily> {
        let mut families: Vec<GpuFamily> = (1..=n).map(GpuFamily::Apple).collect();
        families.push(GpuFamily::Mac2);
        families.push(GpuFamily::Metal3);
        families
    }

    #[test]
    fn gpu_family_raw_values_match_mtlgpufamily() {
        assert_eq!(GpuFamily::Apple(1).raw(), 1001);
        assert_eq!(GpuFamily::Apple(7).raw(), 1007);
        assert_eq!(GpuFamily::Apple(MAX_KNOWN_APPLE_FAMILY).raw(), 1010);
        assert_eq!(GpuFamily::Mac2.raw(), 2002);
        assert_eq!(GpuFamily::Metal3.raw(), 5001);
    }

    #[test]
    fn probe_reports_the_highest_supported_apple_family() {
        let caps = probe("Apple M4 Pro", true, &apple_up_to(9));
        assert_eq!(caps.apple_family, Some(9));
        assert!(caps.mac2);
        assert!(caps.metal3);
        assert!(caps.unified_memory);
        assert_eq!(caps.admit(), Ok(()));
    }

    #[test]
    fn probe_does_not_assume_apple_families_are_cumulative() {
        let caps = probe("odd", true, &[GpuFamily::Apple(8), GpuFamily::Apple(3)]);
        assert_eq!(caps.apple_family, Some(8));
    }

    #[test]
    fn probe_scans_every_known_apple_family() {
        for n in 1..=MAX_KNOWN_APPLE_FAMILY {
            let caps = probe("gpu", true, &[GpuFamily::Apple(n)]);
            assert_eq!(caps.apple_family, Some(n), "Apple{n}");
        }
    }

    #[test]
    fn admit_accepts_the_minimum_family_and_newer() {
        for n in MIN_APPLE_FAMILY..=MAX_KNOWN_APPLE_FAMILY {
            assert_eq!(
                probe("gpu", true, &apple_up_to(n)).admit(),
                Ok(()),
                "Apple{n}"
            );
        }
    }

    #[test]
    fn admit_rejects_older_apple_families_with_the_family_in_the_reason() {
        let caps = probe("Apple A13", true, &apple_up_to(MIN_APPLE_FAMILY - 1));
        let err = caps.admit().unwrap_err();
        assert_eq!(
            err,
            MetalUnavailable::UnsupportedGpuFamily {
                device: "Apple A13".to_string(),
                apple_family: Some(MIN_APPLE_FAMILY - 1),
            }
        );
        assert_eq!(err.code(), "unsupported_gpu_family");
        let text = err.to_string();
        assert!(text.contains("Apple A13"), "{text}");
        assert!(text.contains("MTLGPUFamilyApple6"), "{text}");
        assert!(text.contains("Apple7 or newer"), "{text}");
    }

    #[test]
    fn admit_rejects_intel_and_discrete_mac_gpus() {
        let caps = probe(
            "AMD Radeon Pro 5500M",
            false,
            &[GpuFamily::Mac2, GpuFamily::Metal3],
        );
        assert_eq!(caps.apple_family, None);
        assert!(caps.mac2);
        let err = caps.admit().unwrap_err();
        assert_eq!(err.code(), "unsupported_gpu_family");
        assert!(err.to_string().contains("not an Apple-family GPU"), "{err}");
    }

    #[test]
    fn capabilities_display_names_device_family_and_features() {
        let caps = probe("Apple M4 Pro", true, &apple_up_to(9));
        assert_eq!(
            caps.to_string(),
            "Apple M4 Pro (Apple9, Metal3, Mac2, unified memory)"
        );
        let intel = probe("Intel UHD Graphics 630", false, &[GpuFamily::Mac2]);
        assert_eq!(
            intel.to_string(),
            "Intel UHD Graphics 630 (no Apple family, Mac2)"
        );
    }

    #[test]
    fn unavailable_codes_are_stable_and_distinct() {
        let reasons = [
            MetalUnavailable::UnsupportedPlatform,
            MetalUnavailable::NoDevice,
            MetalUnavailable::UnsupportedGpuFamily {
                device: "d".into(),
                apple_family: None,
            },
            MetalUnavailable::NoCommandQueue { device: "d".into() },
            MetalUnavailable::NotMainThread,
            MetalUnavailable::UnsupportedWindowHandle { kind: "k".into() },
            MetalUnavailable::NoBackingLayer,
            MetalUnavailable::LayerNotMetal {
                class: "CALayer".into(),
            },
        ];
        let codes: Vec<&str> = reasons.iter().map(MetalUnavailable::code).collect();
        assert_eq!(
            codes,
            [
                "unsupported_platform",
                "no_device",
                "unsupported_gpu_family",
                "no_command_queue",
                "not_main_thread",
                "unsupported_window_handle",
                "no_backing_layer",
                "layer_not_metal",
            ]
        );
        for reason in &reasons {
            assert!(!reason.to_string().is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn non_appkit_window_handles_are_refused_with_their_kind() {
        let web =
            raw_window_handle::RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(1));
        let err = appkit_view(web).unwrap_err();
        assert_eq!(
            err,
            MetalUnavailable::UnsupportedWindowHandle {
                kind: "a web canvas".to_string()
            }
        );
        let xcb = raw_window_handle::RawWindowHandle::Xcb(raw_window_handle::XcbWindowHandle::new(
            NonZeroU32::new(7).unwrap(),
        ));
        let err = appkit_view(xcb).unwrap_err();
        assert_eq!(err.code(), "unsupported_window_handle");
        assert!(err.to_string().contains("an XCB window"), "{err}");
    }

    #[test]
    fn appkit_window_handles_yield_their_view_pointer() {
        let view = std::ptr::NonNull::<std::ffi::c_void>::dangling();
        let handle = raw_window_handle::RawWindowHandle::AppKit(
            raw_window_handle::AppKitWindowHandle::new(view),
        );
        assert_eq!(appkit_view(handle), Ok(view));
    }

    #[test]
    fn clear_color_premultiplies_straight_alpha() {
        let color = ClearColor::from_srgba(1.0, 0.5, 0.25, 0.5);
        assert_eq!(
            color,
            ClearColor {
                red: 0.5,
                green: 0.25,
                blue: 0.125,
                alpha: 0.5,
            }
        );
    }

    #[test]
    fn clear_color_clamps_out_of_range_and_nan_components() {
        let color = ClearColor::from_srgba(2.0, -1.0, f32::NAN, 1.5);
        assert_eq!(
            color,
            ClearColor {
                red: 1.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            }
        );
        let transparent = ClearColor::from_srgba(0.3, 0.6, 0.9, f32::NAN);
        assert_eq!(transparent.to_bgra8(), [0, 0, 0, 0]);
    }

    #[test]
    fn clear_color_bgra8_bytes_are_in_bgra_order_and_exact() {
        let color = ClearColor::from_srgba(
            f32::from(0x1e_u8) / 255.0,
            f32::from(0x2a_u8) / 255.0,
            f32::from(0xc8_u8) / 255.0,
            1.0,
        );
        assert_eq!(color.to_bgra8(), [0xc8, 0x2a, 0x1e, 0xff]);
        assert_eq!(
            ClearColor::from_srgba(1.0, 1.0, 1.0, 1.0).to_bgra8(),
            [255; 4]
        );
        assert_eq!(
            ClearColor::from_srgba(0.0, 0.0, 0.0, 0.0).to_bgra8(),
            [0; 4]
        );
    }

    #[test]
    fn frame_errors_describe_their_cause() {
        assert!(
            FrameError::InvalidExtent {
                width: 0,
                height: 9
            }
            .to_string()
            .contains("0x9")
        );
        assert!(
            FrameError::AllocationFailed {
                what: "readback buffer",
                bytes: 64,
            }
            .to_string()
            .contains("64-byte readback buffer")
        );
        assert!(
            FrameError::DrawableUnavailable
                .to_string()
                .contains("drawable")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_targets_report_unsupported_platform() {
        assert_eq!(
            MetalDevice::system_default().err(),
            Some(MetalUnavailable::UnsupportedPlatform)
        );
    }
}
