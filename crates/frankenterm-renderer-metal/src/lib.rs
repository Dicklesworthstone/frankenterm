//! Native Metal renderer for the FrankenTerm GUI on macOS.
//!
//! This crate is the Track C foundation of the mac-render campaign (bead
//! `ft-yccm0.4.1.1`). The GUI owns the render state machine; this crate owns
//! the Metal objects: the [`MetalDevice`] (device, command queue, capability
//! probe) and the [`MetalRenderer`] bound to a window's `CAMetalLayer`.
//!
//! A frame is one render pass: it clears the drawable to the window
//! background color, draws the background pass ([`cell_bg`], ft-yccm0.4.2.2)
//! and then the text pass ([`cell_text`], ft-yccm0.4.2.3), every glyph in one
//! instanced draw that samples the renderer's glyph atlases ([`atlas`]). The
//! GUI does not feed it terminal content yet (the snapshot adapter is
//! ft-yccm0.4.4), so `front_end = "Metal"` is an in-development opt-in.
//!
//! # Frame slots (ft-yccm0.4.2.1)
//!
//! Every frame is written into one of [`FRAME_SLOTS`] frame slots: per-frame
//! buffers (uniforms, cell backgrounds, glyph instances, per-row tables) in
//! shared, write-combined memory that the GPU reads in place. The [`frame`]
//! module paces them, at most three frames in flight, a slot never rewritten
//! before the GPU has finished with it, and sizes them for the grid,
//! growing geometrically so a steady-state frame allocates nothing. On
//! macOS 15+ one residency set holds every slot buffer. On macOS 26 frames are
//! submitted through Metal 4: a command allocator, command buffer and argument
//! table per slot, created once and reused. Elsewhere they go through Metal 3
//! with the same buffer model ([`SubmissionPath`]).
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
//! naming its category below. All blocks live in `src/macos.rs`,
//! `src/macos_frames.rs`, `src/macos_atlas.rs` and
//! `src/macos_display_link.rs`. The non-macOS stub and the
//! portable [`frame`], [`atlas`], [`cell_bg`] and [`cell_text`] modules
//! contain none.
//!
//! Most Metal and Core Animation calls are safe in `objc2-metal` /
//! `objc2-quartz-core` 0.3, whose generated bindings already encode the
//! Objective-C signatures and retain/release rules. The remaining unsafe
//! operations fall into these categories:
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
//!    The glyph atlases (ft-yccm0.4.3.2) also copy texture to texture when
//!    an atlas grows: the source region is the old texture's whole extent and
//!    the destination is the same width and taller, at the same origin; the
//!    atlas test readback copies a glyph's own region at its format's pitch.
//! 4. **BUFFER-CONTENTS: reading shared GPU memory.** Reading back the bytes
//!    of a `MTLStorageModeShared` buffer through `-contents`.
//!    *Invariant:* the pointer covers `-length` bytes, which is at least the
//!    requested length (checked); the command buffer that wrote it has been
//!    waited on to completion and checked for success, so no GPU write races
//!    the read; the bytes are copied into an owned `Vec` before the buffer is
//!    released, so no reference outlives the allocation.
//! 5. **BUFFER-WRITE: writing a frame slot.** Copying a frame's bytes into a
//!    slot buffer's `-contents`.
//!    *Invariant:* the write requires a `SlotLease` from the same slot ring
//!    (asserted), and a lease exists only while its slot is not in flight:
//!    the GPU finished the last frame that read the slot before the lease was
//!    granted. The destination range is checked against `-length`, the
//!    buffer is CPU-visible shared storage, and the source is Rust memory
//!    that cannot overlap it.
//! 6. **FFI-BLOCK: completion handlers.** `addCompletedHandler:` (Metal 3)
//!    and `addFeedbackHandler:` (Metal 4) take an Objective-C block, and the
//!    handler dereferences the command buffer or feedback object Metal passes.
//!    *Invariant:* Metal copies the block, so the local `RcBlock` may drop
//!    after the call; the closure owns only `Send + Sync + 'static` state (a
//!    `CompletionToken`, an `Arc<AtomicU64>`) because Metal runs it on a
//!    thread of its choosing; the handler is added before the commit; the
//!    object Metal passes is valid for the duration of the handler.
//! 7. **FFI-COMMIT: Metal 4 commit.** `commit:count:options:` takes a C
//!    array of command buffers.
//!    *Invariant:* the pointer addresses a live one-element array for the
//!    duration of the call and the count is 1; the command buffer has ended
//!    encoding, and its allocator is not reset until the frame's feedback
//!    handler has freed the slot.
//! 8. **FFI-ARGTABLE: Metal 4 argument tables.** `setAddress:atIndex:` binds
//!    a buffer by GPU address; `setTexture:atIndex:` binds a glyph atlas by
//!    resource ID (ft-yccm0.4.2.3).
//!    *Invariant:* the address belongs to a live slot buffer that the frame
//!    slots own and keep in the queue's residency set; the index is below the
//!    table's `maxBufferBindCount`; a slot's buffers are rebound whenever its
//!    generation changes (a buffer was replaced), before the slot's next frame.
//!    An atlas texture is live and in the atlases' residency set, which joined
//!    the queue; it is rebound every text frame (growth replaces it), at an
//!    index below `maxTextureBindCount`, and a replaced texture is retired
//!    only after every frame that may sample it has finished.
//! 9. **FFI-DRAW: binding and drawing the background and text passes.**
//!    `setVertexBuffer:offset:atIndex:`, `setFragmentBuffer:offset:atIndex:`
//!    and `setFragmentTexture:atIndex:` (Metal 3), and
//!    `drawPrimitives:vertexStart:vertexCount:` and its `instanceCount:` form
//!    (both paths).
//!    *Invariant:* each bound buffer is a live slot buffer of the leased
//!    slot, bound from offset 0 at its `SlotBuffer` index (the shader's
//!    `[[buffer(n)]]`, below Metal's 31 slots); each bound texture is a live
//!    atlas texture at the shader's `[[texture(n)]]`. The background draw is
//!    three vertices of one triangle, and its vertex shader reads no vertex
//!    buffer; it indexes CellBg only below `rows * cols`, and the frame slots
//!    size that buffer for the frame's grid before the frame is encoded. The
//!    text draw is four strip vertices per instance; its vertex shader reads
//!    CellText only below the instance count, every one of which the frame
//!    wrote into a buffer fitted for them first.
//! 10. **ATLAS-UPLOAD: writing glyph pixels into an atlas texture.**
//!     `replaceRegion:mipmapLevel:withBytes:bytesPerRow:` on a glyph atlas
//!     (ft-yccm0.4.3.2).
//!     *Invariant:* the region comes from the atlas's own allocator, so it lies
//!     inside the texture, and the allocator hands out only never-used space
//!     or a page evicted after its last frame finished, so no unfinished frame
//!     reads it; the pixel slice was checked to hold exactly width x height x
//!     bytes-per-pixel bytes at a tight row pitch and is borrowed for the
//!     call; the texture is in shared storage, which the CPU may write.
//! 11. **THREAD-HANDOFF: moving a renderer to its render thread.**
//!     `MetalRendererHandoff` is `Send` (ft-yccm0.4.1.2). A window's renderer
//!     is attached on the main thread, as AppKit requires, then moved whole to
//!     the window's render thread, which then does all its drawing.
//!     *Invariant:* the handoff owns the renderer, and the renderer is not
//!     `Sync`, so one thread at a time uses it and none shares it. It holds no
//!     `Rc` and no thread-local state: its Metal device, queue, buffers,
//!     textures and pipeline states are thread-safe Metal objects
//!     (`MTLDevice` is `Send + Sync` in `objc2-metal`); the slot ring it
//!     shares with completion handlers is an `Arc` of atomics. After the
//!     handoff only the receiving thread touches the `CAMetalLayer`, whose
//!     `nextDrawable`, `drawableSize` and drawable presentation may be used
//!     off the main thread; the drawable size is changed inside an explicit
//!     `CATransaction`, since a render thread has no run loop to commit an
//!     implicit one.
//! 12. **DISPLAY-LINK-DELEGATE: the display link's delegate class.**
//!     `macos_display_link` defines an Objective-C subclass of `NSObject`
//!     conforming to `CAMetalDisplayLinkDelegate` (`define_class!`), and
//!     initializes it with `NSObject`'s `init` (ft-yccm0.4.1.3).
//!     *Invariant:* `NSObject` has no subclassing requirements and the class
//!     does not implement `Drop`; its one method has the protocol's selector
//!     `metalDisplayLink:needsUpdate:` and exact argument types; `init` takes
//!     no arguments and returns the initialized object. The class's instance
//!     variable (the last update) is only touched by the render thread, where
//!     the link's callbacks run.
//! 13. **FFI-RUNLOOP: scheduling the display link.**
//!     `addToRunLoop:forMode:` puts the link on a run loop.
//!     *Invariant:* the run loop is the calling thread's own, and that
//!     thread is the only one that runs it or uses the link; the mode is a
//!     valid string private to the render thread.
//! 14. **THREAD-RUNLOOP: interrupting the render thread's run loop.**
//!     `RunLoopInterrupter` is `Send + Sync` and calls
//!     `CFRunLoopPerformBlock`.
//!     *Invariant:* it uses the run loop only for `CFRunLoopPerformBlock`
//!     and `CFRunLoopWakeUp`, which Core Foundation documents as callable
//!     from any thread; its mode is an immutable `CFString`, and the block is
//!     a valid, non-null block that Core Foundation copies.
//!
//! Thread affinity: `MetalRenderer` holds Objective-C objects that are not
//! `Send`, so it stays on the thread that created it unless it is moved
//! through a [`MetalRendererHandoff`] (category 11).

use std::fmt;
use std::time::Duration;

pub mod atlas;
pub mod cell_bg;
pub mod cell_text;
pub mod frame;
pub mod uploads;
pub use atlas::{AtlasError, AtlasKind, AtlasSlot};
pub use cell_bg::{BackgroundUniforms, CellBg, CellBgGrid, CursorShape, CursorUniform};
pub use cell_text::{CellText, CellTextGrid, TextUniforms, UnderlineStyle};
pub use frame::{FRAME_SLOTS, GridExtent};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_atlas;
#[cfg(target_os = "macos")]
pub use macos_atlas::GlyphAtlases;
#[cfg(target_os = "macos")]
mod macos_display_link;
#[cfg(target_os = "macos")]
pub use macos_display_link::{LinkUpdate, LinkWait, MetalDisplayLink, RunLoopInterrupter};
#[cfg(target_os = "macos")]
mod macos_frames;
#[cfg(target_os = "macos")]
pub use macos::{MetalDevice, MetalRenderer, MetalRendererHandoff};

#[cfg(not(target_os = "macos"))]
mod stub;
#[cfg(not(target_os = "macos"))]
pub use stub::{
    LinkUpdate, LinkWait, MetalDevice, MetalDisplayLink, MetalRenderer, MetalRendererHandoff,
    RunLoopInterrupter,
};

/// Lowest `MTLGPUFamilyAppleN` the renderer admits: Apple7, the M1 generation
/// and the first Apple-silicon Mac family.
pub const MIN_APPLE_FAMILY: u8 = 7;

/// Highest `MTLGPUFamilyAppleN` the pinned `objc2-metal` 0.3.2 bindings name.
pub const MAX_KNOWN_APPLE_FAMILY: u8 = 10;

/// Largest texture width or height accepted for offscreen rendering: the 2D
/// texture limit of every Apple3-or-newer GPU family.
pub const MAX_TEXTURE_EXTENT: u32 = 16_384;

/// Longest a frame waits for a frame slot. A slot frees as soon as the GPU
/// finishes the frame three back, a few milliseconds at most when the GPU is
/// healthy; a frame that waits this long fails with
/// [`FrameError::FrameSlotTimeout`] and the GUI retries it.
pub const FRAME_SLOT_TIMEOUT: Duration = Duration::from_millis(100);

/// Environment variable that selects the submission path: `metal3`, `metal4`
/// or `auto` (the default: Metal 4 where supported).
pub const SUBMISSION_ENV: &str = "FRANKENTERM_METAL_SUBMISSION";

/// How the renderer submits frames to the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionPath {
    /// Metal 4 (macOS 26): `MTL4CommandQueue`, per-slot command allocators,
    /// command buffers and argument tables, residency sets.
    Metal4,
    /// Metal 3: a command buffer per frame and completion handlers, with the
    /// same frame-slot buffers.
    Metal3,
}

/// The submission path [`SubmissionPath::select`] picked, and why it differs
/// from what was asked for, if it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionChoice {
    pub path: SubmissionPath,
    pub note: Option<String>,
}

impl SubmissionPath {
    /// Stable lowercase name, as accepted by [`SUBMISSION_ENV`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Metal4 => "metal4",
            Self::Metal3 => "metal3",
        }
    }

    /// Picks the path from whether the device and OS support Metal 4 and the
    /// [`SUBMISSION_ENV`] value, if set.
    #[must_use]
    pub fn select(metal4_supported: bool, requested: Option<&str>) -> SubmissionChoice {
        let auto = if metal4_supported {
            Self::Metal4
        } else {
            Self::Metal3
        };
        match requested.map(str::trim) {
            None | Some("" | "auto") => SubmissionChoice {
                path: auto,
                note: None,
            },
            Some("metal3") => SubmissionChoice {
                path: Self::Metal3,
                note: metal4_supported
                    .then(|| format!("{SUBMISSION_ENV}=metal3 forces the Metal 3 path")),
            },
            Some("metal4") => SubmissionChoice {
                path: auto,
                note: (!metal4_supported).then(|| {
                    format!("{SUBMISSION_ENV}=metal4, but this device or OS has no Metal 4; using Metal 3")
                }),
            },
            Some(other) => SubmissionChoice {
                path: auto,
                note: Some(format!(
                    "{SUBMISSION_ENV}={other:?} is not metal3, metal4 or auto; using {}",
                    auto.as_str()
                )),
            },
        }
    }
}

impl fmt::Display for SubmissionPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Frame pacing and allocation counters of a [`MetalRenderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameStats {
    pub path: SubmissionPath,
    /// Frames the GPU finished.
    pub frames_completed: u64,
    /// Frames whose command buffer finished in error.
    pub frames_failed: u64,
    /// Frames committed and not yet finished.
    pub in_flight: usize,
    /// The most frames ever in flight at once (at most [`FRAME_SLOTS`]).
    pub peak_in_flight: usize,
    /// Frame-slot buffers created since attach; constant in steady state.
    pub buffer_allocations: u64,
    /// Allocations in the frame slots' residency set, if the OS has them.
    pub resident_allocations: Option<usize>,
}

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
    /// The frame-slot buffers could not be allocated.
    ResourceAllocation { detail: String },
    /// The background pass's shader or pipeline could not be built.
    Pipeline { detail: String },
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
            Self::ResourceAllocation { .. } => "resource_allocation",
            Self::Pipeline { .. } => "pipeline",
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
            Self::ResourceAllocation { detail } => {
                write!(f, "the Metal frame slots could not be allocated: {detail}")
            }
            Self::Pipeline { detail } => {
                write!(f, "the Metal background pass is unusable: {detail}")
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
    /// No frame slot freed within [`FRAME_SLOT_TIMEOUT`]: the GPU has not
    /// finished the frame that last used the next slot.
    FrameSlotTimeout { slot: usize, in_flight: usize },
    /// The grid's per-frame buffers would not fit in memory sizes.
    GridTooLarge { rows: u32, cols: u32 },
    /// A write would run past the end of a frame-slot buffer.
    SlotWriteOutOfBounds {
        buffer: &'static str,
        offset: usize,
        len: usize,
        capacity: usize,
    },
    /// A frame's glyph instances do not belong to its cell grid: another
    /// extent, or another ring offset (ft-yccm0.4.2.3).
    TextGridMismatch {
        cells: GridExtent,
        text: GridExtent,
        cells_row_offset: u32,
        text_row_offset: u32,
    },
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
            Self::FrameSlotTimeout { slot, in_flight } => write!(
                f,
                "frame slot {slot} was not released within {} ms ({in_flight} frames in flight)",
                FRAME_SLOT_TIMEOUT.as_millis()
            ),
            Self::GridTooLarge { rows, cols } => {
                write!(
                    f,
                    "a {rows}x{cols} grid is too large for frame-slot buffers"
                )
            }
            Self::SlotWriteOutOfBounds {
                buffer,
                offset,
                len,
                capacity,
            } => write!(
                f,
                "a {len}-byte write at offset {offset} overruns the {capacity}-byte {buffer} buffer"
            ),
            Self::TextGridMismatch {
                cells,
                text,
                cells_row_offset,
                text_row_offset,
            } => write!(
                f,
                "the glyph grid ({}x{}, ring offset {text_row_offset}) does not match the cell \
                 grid ({}x{}, ring offset {cells_row_offset})",
                text.rows, text.cols, cells.rows, cells.cols
            ),
        }
    }
}

impl std::error::Error for FrameError {}

/// Everything one frame draws (ft-yccm0.4.2.3): the background pass over
/// `cells`, then the text pass over `text`, whose glyphs sample the
/// renderer's atlases. `text` must have `cells`' extent and ring offset.
#[derive(Debug, Clone, Copy)]
pub struct FrameScene<'a> {
    pub cells: &'a CellBgGrid,
    pub background: BackgroundUniforms,
    pub text: &'a CellTextGrid,
    pub text_uniforms: TextUniforms,
    pub clear: ClearColor,
}

impl FrameScene<'_> {
    /// [`FrameError::TextGridMismatch`] unless the glyph grid belongs to the
    /// cell grid.
    pub fn check(&self) -> Result<(), FrameError> {
        let (cells, text) = (self.cells.extent(), self.text.extent());
        if cells == text && self.cells.row_offset() == self.text.row_offset() {
            return Ok(());
        }
        Err(FrameError::TextGridMismatch {
            cells,
            text,
            cells_row_offset: self.cells.row_offset(),
            text_row_offset: self.text.row_offset(),
        })
    }
}

/// The outcome of a successfully encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// A drawable was cleared, scheduled for presentation and committed.
    Presented,
    /// The window has a zero-sized extent, so there was nothing to draw.
    ZeroSize,
    /// The drawable a display link handed over predates a resize
    /// (ft-yccm0.4.1.3): nothing was drawn into it, the layer is sized for
    /// the frame, and the link's next drawable has that size.
    DrawableResized,
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

    /// The premultiplied components as `[red, green, blue, alpha]` `f32`s,
    /// for shader uniforms.
    #[must_use]
    // Components are in 0.0..=1.0, so narrowing to f32 loses only precision.
    #[allow(clippy::cast_possible_truncation)]
    pub fn to_f32(self) -> [f32; 4] {
        [
            self.red as f32,
            self.green as f32,
            self.blue as f32,
            self.alpha as f32,
        ]
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
            MetalUnavailable::ResourceAllocation { detail: "d".into() },
            MetalUnavailable::Pipeline { detail: "d".into() },
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
                "resource_allocation",
                "pipeline",
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

    #[test]
    fn submission_defaults_to_metal4_where_supported_and_honors_the_override() {
        let pick = |supported, requested| SubmissionPath::select(supported, requested);
        assert_eq!(pick(true, None).path, SubmissionPath::Metal4);
        assert_eq!(pick(true, None).note, None);
        assert_eq!(pick(false, None).path, SubmissionPath::Metal3);
        assert_eq!(pick(true, Some("auto")).path, SubmissionPath::Metal4);
        assert_eq!(pick(true, Some(" ")).path, SubmissionPath::Metal4);
        let forced = pick(true, Some("metal3"));
        assert_eq!(forced.path, SubmissionPath::Metal3);
        assert!(forced.note.unwrap().contains("forces the Metal 3 path"));
        assert_eq!(pick(false, Some("metal3")).note, None);
        let impossible = pick(false, Some("metal4"));
        assert_eq!(impossible.path, SubmissionPath::Metal3);
        assert!(impossible.note.unwrap().contains("no Metal 4"));
        assert_eq!(pick(true, Some("metal4")), pick(true, None));
        let typo = pick(true, Some("metl4"));
        assert_eq!(typo.path, SubmissionPath::Metal4);
        assert!(
            typo.note
                .unwrap()
                .contains("\"metl4\" is not metal3, metal4 or auto")
        );
        assert_eq!(SubmissionPath::Metal4.to_string(), "metal4");
        assert_eq!(SubmissionPath::Metal3.as_str(), "metal3");
    }

    #[test]
    fn frame_slot_errors_describe_their_cause() {
        let timeout = FrameError::FrameSlotTimeout {
            slot: 2,
            in_flight: 3,
        }
        .to_string();
        assert!(timeout.contains("frame slot 2"), "{timeout}");
        assert!(timeout.contains("100 ms"), "{timeout}");
        assert!(timeout.contains("3 frames in flight"), "{timeout}");
        assert!(
            FrameError::GridTooLarge { rows: 9, cols: 7 }
                .to_string()
                .contains("9x7 grid")
        );
        let overrun = FrameError::SlotWriteOutOfBounds {
            buffer: "uniforms",
            offset: 250,
            len: 8,
            capacity: 256,
        }
        .to_string();
        assert!(
            overrun.contains("8-byte write at offset 250 overruns the 256-byte uniforms buffer"),
            "{overrun}"
        );
        assert!(
            MetalUnavailable::ResourceAllocation {
                detail: "no memory".into()
            }
            .to_string()
            .contains("frame slots could not be allocated: no memory")
        );
    }

    #[test]
    fn clear_color_f32_components_keep_premultiplication() {
        assert_eq!(
            ClearColor::from_srgba(1.0, 0.5, 0.25, 0.5).to_f32(),
            [0.5, 0.25, 0.125, 0.5]
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

    /// ft-yccm0.4.2.3: a frame's glyph grid must have its cell grid's extent
    /// and ring offset, or the shader would place glyphs on the wrong rows.
    #[test]
    fn frame_scenes_refuse_a_glyph_grid_from_another_cell_grid() {
        fn scene<'a>(cells: &'a CellBgGrid, text: &'a CellTextGrid) -> FrameScene<'a> {
            FrameScene {
                cells,
                background: BackgroundUniforms::default(),
                text,
                text_uniforms: TextUniforms::default(),
                clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            }
        }
        let extent = GridExtent::new(4, 6);
        let mut cells = CellBgGrid::new(extent);
        let mut text = CellTextGrid::new(extent);
        assert_eq!(scene(&cells, &text).check(), Ok(()));
        cells.scroll_up(1);
        let error = scene(&cells, &text).check().unwrap_err();
        assert_eq!(
            error,
            FrameError::TextGridMismatch {
                cells: extent,
                text: extent,
                cells_row_offset: 1,
                text_row_offset: 0,
            }
        );
        assert!(error.to_string().contains("ring offset 0"), "{error}");
        text.scroll_up(1);
        assert_eq!(scene(&cells, &text).check(), Ok(()));
        let other = CellTextGrid::new(GridExtent::new(4, 7));
        assert!(matches!(
            scene(&cells, &other).check(),
            Err(FrameError::TextGridMismatch { .. })
        ));
    }
}
