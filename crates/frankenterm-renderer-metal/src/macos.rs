//! The macOS implementation. Every `unsafe` block in the crate is here, and
//! each names its UNSAFE-CONTRACT category from the crate docs.

use crate::atlas::{AtlasConfig, AtlasError, AtlasKind, AtlasSlot, FrameFence};
use crate::cell_bg::{BackgroundUniforms, CellBgGrid};
use crate::cell_text::{CellTextGrid, TextUniforms};
use crate::frame::{FrameUniforms, GridExtent, SlotBuffer, UNIFORMS_BYTES};
use crate::macos_atlas::GlyphAtlases;
use crate::macos_display_link::{LinkUpdate, MetalDisplayLink};
use crate::macos_frames::{
    BackgroundPipeline, EncodedWindow, FrameSlots, OffscreenCells, OffscreenText, PaneTextDraw,
    Submission, TextDraw, TextPipeline, UiDraw, UiPipeline, WindowDraw, supports_metal4,
};
use crate::{
    ClearColor, DeviceCapabilities, FRAME_SLOT_TIMEOUT, FrameError, FrameOutcome, FrameScene,
    FrameStats, MAX_TEXTURE_EXTENT, MetalUnavailable, SUBMISSION_ENV, SubmissionPath, WindowFrame,
    appkit_view,
};
use frankenterm_alloc::resource_ledger::GpuResourceLedger;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{MainThreadMarker, msg_send};
use objc2_core_foundation::CGSize;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable,
    MTLGPUFamily, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLRenderPassDescriptor,
    MTLResidencySet, MTLResourceOptions, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CALayer, CAMetalDrawable, CAMetalLayer, CATransaction};
use raw_window_handle::HasWindowHandle;
use std::cell::{Cell, RefCell};
use std::fmt;

/// The pixel format of every target this crate renders to.
const PIXEL_FORMAT: MTLPixelFormat = crate::macos_frames::TARGET_PIXEL_FORMAT;
const BYTES_PER_PIXEL: usize = 4;

/// A Metal device admitted for rendering, with its command queue.
pub struct MetalDevice {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    capabilities: DeviceCapabilities,
}

impl fmt::Debug for MetalDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetalDevice")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl MetalDevice {
    /// Opens the system default device, probes its GPU families, admits it
    /// (Apple7 or newer) and creates its command queue.
    pub fn system_default() -> Result<Self, MetalUnavailable> {
        let device = MTLCreateSystemDefaultDevice().ok_or(MetalUnavailable::NoDevice)?;
        let capabilities = DeviceCapabilities::from_family_probe(
            device.name().to_string(),
            device.hasUnifiedMemory(),
            |family| device.supportsFamily(MTLGPUFamily(family.raw())),
        );
        capabilities.admit()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| MetalUnavailable::NoCommandQueue {
                device: capabilities.name.clone(),
            })?;
        Ok(Self {
            device,
            queue,
            capabilities,
        })
    }

    /// What the capability probe learned about this device.
    #[must_use]
    pub fn capabilities(&self) -> &DeviceCapabilities {
        &self.capabilities
    }

    pub(crate) fn raw_device(&self) -> Retained<ProtocolObject<dyn MTLDevice>> {
        self.device.clone()
    }

    /// The device's command queue; the glyph atlases blit on it when they
    /// grow (ft-yccm0.4.3.2).
    pub(crate) fn raw_queue(&self) -> &ProtocolObject<dyn MTLCommandQueue> {
        &self.queue
    }

    /// Clears an offscreen `width x height` `BGRA8Unorm_sRGB` texture with one
    /// render pass, waits for the GPU, and returns the texture's bytes:
    /// row-major, tightly packed, `[B, G, R, A]` per pixel.
    pub fn clear_offscreen(
        &self,
        width: u32,
        height: u32,
        color: ClearColor,
    ) -> Result<Vec<u8>, FrameError> {
        if !(1..=MAX_TEXTURE_EXTENT).contains(&width) || !(1..=MAX_TEXTURE_EXTENT).contains(&height)
        {
            return Err(FrameError::InvalidExtent { width, height });
        }
        let (width_px, height_px) = (width as usize, height as usize);
        let bytes_per_row = width_px * BYTES_PER_PIXEL;
        let len = bytes_per_row * height_px;

        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. BGRA8Unorm_sRGB is color-renderable and the
        // extent was checked to be within 1..=MAX_TEXTURE_EXTENT on both axes
        // above.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                PIXEL_FORMAT,
                width_px,
                height_px,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        descriptor.setStorageMode(MTLStorageMode::Private);
        let texture = self.device.newTextureWithDescriptor(&descriptor).ok_or(
            FrameError::AllocationFailed {
                what: "render target texture",
                bytes: len,
            },
        )?;
        let buffer = self
            .device
            .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared)
            .ok_or(FrameError::AllocationFailed {
                what: "readback buffer",
                bytes: len,
            })?;

        let commands = self
            .queue
            .commandBuffer()
            .ok_or(FrameError::CommandBufferUnavailable)?;
        let pass = clear_pass(&texture, color);
        let encoder = commands
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or(FrameError::EncoderUnavailable)?;
        encoder.endEncoding();
        let blit = commands
            .blitCommandEncoder()
            .ok_or(FrameError::EncoderUnavailable)?;
        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. The source region is the texture's whole
        // width x height x 1 extent in slice 0, level 0. The destination pitch
        // is width * 4 bytes for the 4-byte BGRA8 format, and `buffer` holds
        // exactly pitch * height bytes from offset 0.
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &texture,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
                MTLSize {
                    width: width_px,
                    height: height_px,
                    depth: 1,
                },
                &buffer,
                0,
                bytes_per_row,
                len,
            );
        }
        blit.endEncoding();
        commands.commit();
        commands.waitUntilCompleted();
        let status = commands.status();
        if status != MTLCommandBufferStatus::Completed {
            let detail = commands.error().map_or_else(
                || format!("status {}", status.0),
                |error| error.localizedDescription().to_string(),
            );
            return Err(FrameError::CommandFailed { detail });
        }
        if buffer.length() < len {
            return Err(FrameError::AllocationFailed {
                what: "readback buffer",
                bytes: len,
            });
        }
        let contents = buffer.contents().cast::<u8>();
        #[allow(unsafe_code)]
        // SAFETY: BUFFER-CONTENTS. `contents` points to `buffer.length()`
        // bytes of shared storage, checked to be at least `len`. The command
        // buffer that wrote them has completed successfully, so the GPU no
        // longer writes them, and the slice is copied into an owned Vec while
        // `buffer` is still alive.
        let bytes = unsafe { std::slice::from_raw_parts(contents.as_ptr(), len) }.to_vec();
        Ok(bytes)
    }
}

/// The Metal renderer bound to one window's `CAMetalLayer`.
pub struct MetalRenderer {
    device: MetalDevice,
    layer: Retained<CAMetalLayer>,
    drawable_size: Cell<(u32, u32)>,
    frames: RefCell<FrameSlots>,
    submission: Submission,
    submission_note: Option<String>,
    background: BackgroundPipeline,
    text: TextPipeline,
    /// Window chrome quads (ft-yccm0.4.7.1).
    ui: UiPipeline,
    /// The glyph atlases the text pass samples (ft-yccm0.4.2.3). Dropped
    /// after `Drop::drop` has waited out the frames that sample them.
    atlases: RefCell<GlyphAtlases>,
}

/// Longest a dropped renderer waits for its in-flight frames.
const DROP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

impl Drop for MetalRenderer {
    /// Metal 4 command buffers do not retain the resources they use, so the
    /// frame-slot buffers must outlive every committed frame: wait (bounded)
    /// for the GPU to finish before the fields drop.
    fn drop(&mut self) {
        // No panic here: this may run during an unwind.
        self.frames.borrow().ring().wait_idle(DROP_IDLE_TIMEOUT);
    }
}

impl fmt::Debug for MetalRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetalRenderer")
            .field("device", &self.device)
            .field("drawable_size", &self.drawable_size.get())
            .field("submission", &self.submission.path())
            .finish_non_exhaustive()
    }
}

/// A [`MetalRenderer`] on its way to the thread that will draw with it, the
/// window's render thread (ft-yccm0.4.1.2). The renderer is attached on the
/// main thread, as AppKit requires, then moved here whole.
#[derive(Debug)]
pub struct MetalRendererHandoff(MetalRenderer);

#[allow(unsafe_code)]
// SAFETY: THREAD-HANDOFF. The handoff owns its renderer and the renderer is
// not `Sync`, so only the thread holding it uses it. Its Metal objects are
// thread-safe and it holds no `Rc` or thread-local state; after the move only
// the receiving thread touches the layer (crate docs, category 11).
unsafe impl Send for MetalRendererHandoff {}

impl MetalRendererHandoff {
    pub fn new(renderer: MetalRenderer) -> Self {
        Self(renderer)
    }

    /// The renderer, on the thread that will draw with it.
    pub fn into_renderer(self) -> MetalRenderer {
        self.0
    }
}

impl MetalRenderer {
    /// Opens and admits the system default device, then binds it to the
    /// window's backing `CAMetalLayer`. Must be called on the main thread.
    ///
    /// The device is probed and the frame slots are allocated before the
    /// layer is touched, so a refusal leaves the layer exactly as the window
    /// created it for the fallback backend.
    ///
    /// Frames go through Metal 4 when the device and OS support it, else
    /// Metal 3; [`SUBMISSION_ENV`] can force Metal 3.
    pub fn attach(window: &impl HasWindowHandle) -> Result<Self, MetalUnavailable> {
        let parts = RendererParts::prepare()?;
        let handle =
            window
                .window_handle()
                .map_err(|err| MetalUnavailable::UnsupportedWindowHandle {
                    kind: format!("unavailable ({err})"),
                })?;
        let view = appkit_view(handle.as_raw())?;
        if MainThreadMarker::new().is_none() {
            return Err(MetalUnavailable::NotMainThread);
        }
        #[allow(unsafe_code)]
        // SAFETY: FFI-VIEW. The borrowed `handle` keeps the NSView alive for
        // this call, and we are on the main thread (checked above). NSView is
        // an NSObject subclass, so viewing it as NSObject is sound.
        let view: &NSObject = unsafe { view.cast::<NSObject>().as_ref() };
        #[allow(unsafe_code)]
        // SAFETY: FFI-VIEW. `-[NSView layer]` takes no arguments and returns
        // a nullable `CALayer *`, matching the declared return type. The
        // returned layer is retained, so it outlives the view borrow.
        let layer: Option<Retained<CALayer>> = unsafe { msg_send![view, layer] };
        let layer = layer
            .ok_or(MetalUnavailable::NoBackingLayer)?
            .downcast::<CAMetalLayer>()
            .map_err(|layer| MetalUnavailable::LayerNotMetal {
                class: layer.class().name().to_string_lossy().into_owned(),
            })?;
        Ok(parts.bind(layer))
    }

    /// A renderer bound to no window (ft-yccm0.4.4): its own unattached
    /// `CAMetalLayer`, so it only draws offscreen through
    /// [`Self::snapshot_frame`] and [`Self::snapshot_cells`]. Tests and
    /// image-parity checks read exact frames back this way.
    pub fn offscreen() -> Result<Self, MetalUnavailable> {
        Ok(RendererParts::prepare()?.bind(CAMetalLayer::new()))
    }
}

/// Everything a [`MetalRenderer`] owns except its layer.
struct RendererParts {
    device: MetalDevice,
    frames: FrameSlots,
    submission: Submission,
    submission_note: Option<String>,
    background: BackgroundPipeline,
    text: TextPipeline,
    ui: UiPipeline,
    atlases: GlyphAtlases,
}

impl RendererParts {
    /// Opens and admits the system default device, allocates the frame
    /// slots, pipelines and atlases, and picks the submission path.
    fn prepare() -> Result<Self, MetalUnavailable> {
        let device = MetalDevice::system_default()?;
        let frames = FrameSlots::new(
            device.raw_device(),
            GridExtent::default(),
            GpuResourceLedger::global(),
        )
        .map_err(|error| MetalUnavailable::ResourceAllocation {
            detail: error.to_string(),
        })?;
        let background = BackgroundPipeline::new(&device.device)
            .map_err(|detail| MetalUnavailable::Pipeline { detail })?;
        let text = TextPipeline::new(&device.device)
            .map_err(|detail| MetalUnavailable::Pipeline { detail })?;
        let ui = UiPipeline::new(&device.device)
            .map_err(|detail| MetalUnavailable::Pipeline { detail })?;
        // The color atlas is sRGB, so sampling decodes emoji to linear light
        // as the WebGpu atlas does (ft-yccm0.4.7.3).
        let atlases = GlyphAtlases::new(
            &device,
            AtlasConfig::for_kind(AtlasKind::Grayscale),
            AtlasConfig::for_kind(AtlasKind::Color),
            true,
            GpuResourceLedger::global(),
        )
        .map_err(|error| MetalUnavailable::ResourceAllocation {
            detail: error.to_string(),
        })?;
        let choice = SubmissionPath::select(
            supports_metal4(&device.device) && frames.residency().is_some(),
            std::env::var(SUBMISSION_ENV).ok().as_deref(),
        );
        let (submission, submission_note) = match choice.path {
            SubmissionPath::Metal4 => match Submission::metal4(&device.device, &frames) {
                Ok(submission) => (submission, choice.note),
                Err(reason) => (
                    Submission::metal3(device.queue.clone(), &frames),
                    Some(format!(
                        "Metal 4 submission unavailable ({reason}); using Metal 3"
                    )),
                ),
            },
            SubmissionPath::Metal3 => (
                Submission::metal3(device.queue.clone(), &frames),
                choice.note,
            ),
        };
        if let Some(set) = atlases.residency() {
            submission.add_residency_set(set);
        }
        Ok(Self {
            device,
            frames,
            submission,
            submission_note,
            background,
            text,
            ui,
            atlases,
        })
    }

    /// Binds the parts to `layer`, configured for this device.
    fn bind(self, layer: Retained<CAMetalLayer>) -> MetalRenderer {
        layer.setDevice(Some(&self.device.device));
        layer.setPixelFormat(PIXEL_FORMAT);
        layer.setFramebufferOnly(true);
        // ft-yccm0.4.1.3: triple buffering, presentation synced to the
        // display's refresh, set explicitly rather than trusting defaults.
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(true);
        self.submission.add_layer(&layer);
        MetalRenderer {
            device: self.device,
            layer,
            drawable_size: Cell::new((0, 0)),
            frames: RefCell::new(self.frames),
            submission: self.submission,
            submission_note: self.submission_note,
            background: self.background,
            text: self.text,
            ui: self.ui,
            atlases: RefCell::new(self.atlases),
        }
    }
}

impl MetalRenderer {
    /// The admitted device this renderer draws with.
    #[must_use]
    pub fn device(&self) -> &MetalDevice {
        &self.device
    }

    /// How frames reach the GPU.
    #[must_use]
    pub fn submission_path(&self) -> SubmissionPath {
        self.submission.path()
    }

    /// Why the submission path differs from the default choice, if it does.
    #[must_use]
    pub fn submission_note(&self) -> Option<&str> {
        self.submission_note.as_deref()
    }

    /// Frame pacing and allocation counters.
    #[must_use]
    pub fn frame_stats(&self) -> FrameStats {
        let frames = self.frames.borrow();
        let ring = frames.ring();
        let (upload_bytes_last, upload_bytes_total) = frames.upload_bytes();
        FrameStats {
            path: self.submission.path(),
            frames_completed: ring.frames_completed(),
            frames_failed: self.submission.failed_frames(),
            in_flight: ring.in_flight(),
            peak_in_flight: ring.peak_in_flight(),
            buffer_allocations: frames.allocations(),
            resident_allocations: frames.residency().map(MTLResidencySet::allocationCount),
            upload_bytes_last,
            upload_bytes_total,
        }
    }

    /// The layer's drawable size, in pixels, as the last rendered frame set
    /// it; `(0, 0)` before the first.
    #[must_use]
    pub fn drawable_size(&self) -> (u32, u32) {
        self.drawable_size.get()
    }

    /// Renders one frame of a `width x height` pixel layer showing `grid`:
    /// leases the next frame slot (waiting at most [`FRAME_SLOT_TIMEOUT`] for
    /// the GPU to release it), writes the frame's uniforms into it, clears
    /// the next drawable to `color`, schedules its presentation and commits.
    /// Does not wait for the GPU.
    pub fn render_clear(
        &self,
        width: u32,
        height: u32,
        grid: GridExtent,
        color: ClearColor,
    ) -> Result<FrameOutcome, FrameError> {
        self.render(width, height, grid, color, None, None, None)
    }

    /// Renders one frame with the background pass (ft-yccm0.4.2.2): uploads
    /// `cells` (ring order) into the slot's CellBg buffer, writes the
    /// uniforms (`background`, with `row_offset` taken from `cells`), clears
    /// the drawable to `clear` and draws one full-screen triangle that shades
    /// every cell, the selection and search tints and the cursor.
    pub fn render_cells(
        &self,
        width: u32,
        height: u32,
        cells: &CellBgGrid,
        clear: ClearColor,
        background: BackgroundUniforms,
    ) -> Result<FrameOutcome, FrameError> {
        self.render(
            width,
            height,
            cells.extent(),
            clear,
            Some((cells, background)),
            None,
            None,
        )
    }

    /// Renders one frame of `scene` (ft-yccm0.4.2.3): the background pass
    /// over its cells, then every glyph instance of its text in one
    /// instanced draw that samples this renderer's atlases. Its glyphs must
    /// have been placed with [`Self::insert_glyph`] (or kept live with
    /// [`Self::touch_glyph`]) since the previous frame. Does not wait for the
    /// GPU.
    pub fn render_frame(
        &self,
        width: u32,
        height: u32,
        scene: &FrameScene<'_>,
    ) -> Result<FrameOutcome, FrameError> {
        scene.check()?;
        self.render(
            width,
            height,
            scene.cells.extent(),
            scene.clear,
            Some((scene.cells, scene.background)),
            Some((scene.text, scene.text_uniforms)),
            None,
        )
    }

    /// [`Self::render_frame`] into the drawable a display link handed over
    /// (ft-yccm0.4.1.3), instead of waiting for one in `nextDrawable`. A
    /// drawable from before a resize is not drawn into:
    /// [`FrameOutcome::DrawableResized`].
    pub fn render_frame_into(
        &self,
        update: LinkUpdate,
        width: u32,
        height: u32,
        scene: &FrameScene<'_>,
    ) -> Result<FrameOutcome, FrameError> {
        scene.check()?;
        self.render(
            width,
            height,
            scene.cells.extent(),
            scene.clear,
            Some((scene.cells, scene.background)),
            Some((scene.text, scene.text_uniforms)),
            Some(update.drawable),
        )
    }

    /// [`Self::render_clear`] into the drawable a display link handed over.
    pub fn render_clear_into(
        &self,
        update: LinkUpdate,
        width: u32,
        height: u32,
        grid: GridExtent,
        color: ClearColor,
    ) -> Result<FrameOutcome, FrameError> {
        self.render(
            width,
            height,
            grid,
            color,
            None,
            None,
            Some(update.drawable),
        )
    }

    /// A display link for this renderer's layer, paused, on the calling
    /// thread's run loop (ft-yccm0.4.1.3); use it only on that thread.
    /// `None` before macOS 14.
    #[must_use]
    pub fn display_link(&self) -> Option<MetalDisplayLink> {
        MetalDisplayLink::new(&self.layer)
    }

    /// Sets the layer's contents scale to the backing scale, drawable
    /// pixels per layer point for a `width`-pixel drawable, and whether the
    /// layer is opaque (ft-yccm0.4.1.3). The window's shared backing layer
    /// starts non-opaque at scale 1.0 and AppKit resets the scale to 1.0 on a
    /// screen change, so the Metal path sets both itself at every surface
    /// change, inside an explicit transaction.
    pub fn configure_surface(&self, width: u32, opaque: bool) {
        let points = self.layer.bounds().size.width;
        let scale = if points > 0.0 {
            (f64::from(width) / points).round().max(1.0)
        } else {
            1.0
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if (self.layer.contentsScale() - scale).abs() > f64::EPSILON {
            self.layer.setContentsScale(scale);
        }
        if self.layer.isOpaque() != opaque {
            self.layer.setOpaque(opaque);
        }
        CATransaction::commit();
    }

    /// [`Self::render_frame`] into an offscreen `width x height` texture,
    /// returning its bytes (BGRA8, row-major, tightly packed). Waits for the
    /// GPU and presents nothing.
    pub fn snapshot_frame(
        &self,
        width: u32,
        height: u32,
        scene: &FrameScene<'_>,
    ) -> Result<Vec<u8>, FrameError> {
        scene.check()?;
        let atlases = self.atlases.borrow();
        self.frames.borrow_mut().render_cells_offscreen(
            &self.submission,
            &self.background,
            &self.device.queue,
            &OffscreenCells {
                width,
                height,
                cells: scene.cells,
                clear: scene.clear,
                background: scene.background,
                text: Some(OffscreenText {
                    grid: scene.text,
                    uniforms: scene.text_uniforms,
                    pipeline: &self.text,
                    atlases: &atlases,
                }),
            },
        )
    }

    /// Places a `width x height` glyph in `kind`'s atlas for the next frame
    /// and uploads its pixels: one coverage byte per pixel for grayscale,
    /// `[B, G, R, A]` premultiplied for color, rows tightly packed. The
    /// returned slot goes into [`crate::CellText::with_glyph`].
    pub fn insert_glyph(
        &self,
        kind: AtlasKind,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<AtlasSlot, AtlasError> {
        let fence = self.atlas_fence();
        self.atlases
            .borrow_mut()
            .insert(&self.device, kind, width, height, pixels, fence)
    }

    /// Records that the next frame draws the glyph in `slot`. False when the
    /// atlas evicted it: insert the glyph again.
    pub fn touch_glyph(&self, slot: &AtlasSlot) -> bool {
        let frame = self.atlas_fence().current;
        self.atlases.borrow_mut().touch(slot, frame)
    }

    /// The frame being prepared, and the frames the GPU has finished.
    fn atlas_fence(&self) -> FrameFence {
        let frames = self.frames.borrow();
        let ring = frames.ring();
        FrameFence {
            current: ring.next_frame(),
            retired_before: ring.retired_before(),
        }
    }

    /// Renders `cells` with the background pass into an offscreen
    /// `width x height` texture and returns its bytes (BGRA8, row-major,
    /// tightly packed). Waits for the GPU and presents nothing: the render
    /// snapshot of the image-parity corpus (ft-yccm0.1.10) reads back real
    /// cell output this way.
    pub fn snapshot_cells(
        &self,
        width: u32,
        height: u32,
        cells: &CellBgGrid,
        clear: ClearColor,
        background: BackgroundUniforms,
    ) -> Result<Vec<u8>, FrameError> {
        self.frames.borrow_mut().render_cells_offscreen(
            &self.submission,
            &self.background,
            &self.device.queue,
            &OffscreenCells {
                width,
                height,
                cells,
                clear,
                background,
                text: None,
            },
        )
    }

    /// Sizes the layer's drawables for a `width x height` frame.
    fn fit_drawable(&self, width: u32, height: u32) {
        if self.drawable_size.get() != (width, height) {
            // A render thread has no run loop to commit an implicit
            // transaction (ft-yccm0.4.1.2): change the size in an explicit
            // one, without the implicit resize animation.
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.layer
                .setDrawableSize(CGSize::new(f64::from(width), f64::from(height)));
            CATransaction::commit();
            self.drawable_size.set((width, height));
        }
    }

    /// Renders one window frame (ft-yccm0.4.6): every pane of `frame` at its
    /// rectangle and every fill, in one command buffer and render pass. Each
    /// pane uploads only the rows it changed since the frame slot last held
    /// it, so an unchanged pane costs no upload. Does not wait for the GPU.
    pub fn render_window(
        &self,
        width: u32,
        height: u32,
        frame: &WindowFrame<'_>,
    ) -> Result<FrameOutcome, FrameError> {
        self.render_window_with(width, height, frame, None)
    }

    /// [`Self::render_window`] into the drawable a display link handed
    /// over (ft-yccm0.4.1.3).
    pub fn render_window_into(
        &self,
        update: LinkUpdate,
        width: u32,
        height: u32,
        frame: &WindowFrame<'_>,
    ) -> Result<FrameOutcome, FrameError> {
        self.render_window_with(width, height, frame, Some(update.drawable))
    }

    /// [`Self::render_window`] into an offscreen `width x height` texture,
    /// returning its bytes (BGRA8, row-major, tightly packed). Waits for the
    /// GPU and presents nothing.
    pub fn snapshot_window(
        &self,
        width: u32,
        height: u32,
        frame: &WindowFrame<'_>,
    ) -> Result<Vec<u8>, FrameError> {
        frame.check()?;
        let target = self.frames.borrow().offscreen_target(width, height)?;
        let failed_before = self.submission.failed_frames();
        let result = self
            .encode_window_frame(width, height, frame, &target, None)
            .and_then(|()| {
                self.frames.borrow().finish_offscreen(
                    &self.submission,
                    failed_before,
                    &self.device.queue,
                    &target,
                    width,
                    height,
                )
            });
        self.frames.borrow().release_offscreen_target(&target);
        result
    }

    fn render_window_with(
        &self,
        width: u32,
        height: u32,
        frame: &WindowFrame<'_>,
        provided: Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>,
    ) -> Result<FrameOutcome, FrameError> {
        frame.check()?;
        if width == 0 || height == 0 {
            return Ok(FrameOutcome::ZeroSize);
        }
        self.fit_drawable(width, height);
        if let Some(drawable) = &provided {
            let texture = drawable.texture();
            if (texture.width(), texture.height()) != (width as usize, height as usize) {
                return Ok(FrameOutcome::DrawableResized);
            }
        }
        let drawable = match provided {
            Some(drawable) => drawable,
            None => self
                .layer
                .nextDrawable()
                .ok_or(FrameError::DrawableUnavailable)?,
        };
        self.encode_window_frame(
            width,
            height,
            frame,
            &drawable.texture(),
            Some(ProtocolObject::from_ref(&*drawable)),
        )?;
        Ok(FrameOutcome::Presented)
    }

    /// The chrome draw of a window frame that wrote `quads` chrome quads
    /// into `slot` (ft-yccm0.4.7.1); `None` without any.
    fn ui_draw<'a>(
        &'a self,
        frames: &'a FrameSlots,
        slot: usize,
        range: std::ops::Range<usize>,
        [width, height]: [u32; 2],
    ) -> Result<Option<UiDraw<'a>>, FrameError> {
        if range.is_empty() {
            return Ok(None);
        }
        let buffers = frames
            .ui_buffers(slot)
            .ok_or(FrameError::AllocationFailed {
                what: "window chrome buffers",
                bytes: 0,
            })?;
        Ok(Some(UiDraw {
            pipeline: &self.ui,
            buffers,
            first: range.start,
            quads: range.len(),
            scissor: crate::PixelRect::new(0, 0, width, height),
        }))
    }

    /// Uploads `frame`'s panes into the next frame slot, writes one uniform
    /// block per draw, and encodes and commits the frame into `target`.
    fn encode_window_frame(
        &self,
        width: u32,
        height: u32,
        frame: &WindowFrame<'_>,
        target: &ProtocolObject<dyn MTLTexture>,
        drawable: Option<&ProtocolObject<dyn MTLDrawable>>,
    ) -> Result<(), FrameError> {
        let retired_before = self.frames.borrow().ring().retired_before();
        self.atlases.borrow_mut().collect_retired(retired_before);
        // The slot's own grid buffers are not used by a window frame.
        let lease =
            self.frames
                .borrow_mut()
                .begin_frame(GridExtent::new(1, 1), 0, FRAME_SLOT_TIMEOUT)?;
        let mut instances = Vec::with_capacity(frame.panes.len());
        {
            let mut frames = self.frames.borrow_mut();
            frames.begin_window_uploads();
            for pane in frame.panes {
                instances.push(frames.upload_pane(
                    &lease,
                    pane.key,
                    pane.cells,
                    pane.text.map(|(text, _)| text),
                )?);
            }
            frames.retire_panes(lease.frame());
        }
        let placed = window_blocks(frame, lease.frame(), width, height);
        let blocks: Vec<[u8; UNIFORMS_BYTES]> = placed.iter().map(|draw| draw.bytes).collect();
        self.frames.borrow_mut().write_window(&lease, &blocks)?;
        // ft-yccm0.4.7.1: the window chrome, drawn over everything else.
        let ui_quads = match &frame.ui {
            Some(ui) => self
                .frames
                .borrow_mut()
                .write_ui(&lease, ui, [width, height])?,
            None => 0,
        };
        let frames = self.frames.borrow();
        let slot = lease.slot();
        let missing = || FrameError::AllocationFailed {
            what: "window frame buffers",
            bytes: 0,
        };
        let buffers = frames.window_buffers(slot).ok_or_else(missing)?;
        let mut draws = Vec::with_capacity(placed.len());
        for (block, placement) in placed.iter().enumerate() {
            let draw = match placement.pane {
                Some(index) => {
                    let scene = &frame.panes[index];
                    let buffer = |kind| {
                        frames
                            .pane_buffer(slot, scene.key, kind)
                            .ok_or_else(missing)
                    };
                    let text = match scene.text {
                        Some(_) => Some(PaneTextDraw {
                            cell_text: buffer(SlotBuffer::CellText)?,
                            row_table: buffer(SlotBuffer::RowTable)?,
                            instances: instances[index],
                        }),
                        None => None,
                    };
                    WindowDraw {
                        uniforms_offset: block * UNIFORMS_BYTES,
                        scissor: placement.rect,
                        cells: buffer(SlotBuffer::CellBg)?,
                        text,
                        over: false,
                    }
                }
                None => WindowDraw {
                    uniforms_offset: block * UNIFORMS_BYTES,
                    scissor: placement.rect,
                    cells: buffers.fill,
                    text: None,
                    over: true,
                },
            };
            draws.push(draw);
        }
        let under = frame.ui.as_ref().map_or(0, |ui| ui.under.min(ui_quads));
        let ui_under = self.ui_draw(&frames, slot, 0..under, [width, height])?;
        let ui = self.ui_draw(&frames, slot, under..ui_quads, [width, height])?;
        let atlases = self.atlases.borrow();
        self.submission.encode_window(
            &frames,
            lease,
            &EncodedWindow {
                target,
                drawable,
                color: frame.clear,
                uniforms: buffers.uniforms,
                background: &self.background,
                text: TextDraw {
                    pipeline: &self.text,
                    atlases: &atlases,
                    instances: 0,
                },
                draws: &draws,
                ui_under,
                ui,
            },
        )
    }

    // One parameter per frame input; a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        width: u32,
        height: u32,
        grid: GridExtent,
        color: ClearColor,
        cells: Option<(&CellBgGrid, BackgroundUniforms)>,
        text: Option<(&CellTextGrid, TextUniforms)>,
        provided: Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>,
    ) -> Result<FrameOutcome, FrameError> {
        if width == 0 || height == 0 {
            return Ok(FrameOutcome::ZeroSize);
        }
        self.fit_drawable(width, height);
        if let Some(drawable) = &provided {
            // A display link's drawable from before a resize would show
            // this frame stretched: skip it, before leasing a frame slot.
            let texture = drawable.texture();
            let size = (texture.width(), texture.height());
            if size != (width as usize, height as usize) {
                return Ok(FrameOutcome::DrawableResized);
            }
        }
        let retired_before = self.frames.borrow().ring().retired_before();
        self.atlases.borrow_mut().collect_retired(retired_before);
        let lease = self
            .frames
            .borrow_mut()
            .begin_frame(grid, 0, FRAME_SLOT_TIMEOUT)?;
        // ft-yccm0.4.2.4: only the rows that changed since this slot last
        // held the grids are copied into it.
        let text_instances = self.frames.borrow_mut().upload(
            &lease,
            grid,
            cells.map(|(cells, _)| cells),
            text.map(|(text, _)| text),
        )?;
        let frames = self.frames.borrow();
        let mut uniforms = FrameUniforms {
            frame: lease.frame(),
            viewport: [width, height],
            grid,
            clear: color.to_f32(),
            ..FrameUniforms::default()
        };
        if let Some((cells, background)) = cells {
            uniforms.background = BackgroundUniforms {
                row_offset: cells.row_offset(),
                ..background
            };
        }
        if let Some((_, text_uniforms)) = text {
            uniforms.text = text_uniforms;
        }
        frames.write(&lease, SlotBuffer::Uniforms, 0, &uniforms.to_bytes())?;
        // A frame abandoned here drops its lease, which frees the slot.
        let drawable = match provided {
            Some(drawable) => drawable,
            None => self
                .layer
                .nextDrawable()
                .ok_or(FrameError::DrawableUnavailable)?,
        };
        let atlases = self.atlases.borrow();
        self.submission.encode_frame(
            &frames,
            lease,
            &drawable.texture(),
            Some(ProtocolObject::from_ref(&*drawable)),
            color,
            cells.map(|_| &self.background),
            text.map(|_| TextDraw {
                pipeline: &self.text,
                atlases: &atlases,
                instances: text_instances,
            }),
        )?;
        Ok(FrameOutcome::Presented)
    }
}

/// One draw of a window frame, placed: its uniform block, its scissor, and
/// the index of its pane, or `None` for a fill.
struct PlacedBlock {
    bytes: [u8; UNIFORMS_BYTES],
    rect: crate::PixelRect,
    pane: Option<usize>,
}

/// The uniform blocks of `frame`'s panes, then of its fills, in draw order;
/// whatever lies wholly outside the `width x height` drawable is left out.
// Fill rectangles are drawable pixel sizes, exact in f32.
#[allow(clippy::cast_precision_loss)]
fn window_blocks(
    frame: &WindowFrame<'_>,
    frame_index: u64,
    width: u32,
    height: u32,
) -> Vec<PlacedBlock> {
    let viewport = [width, height];
    let mut placed = Vec::with_capacity(frame.panes.len() + frame.fills.len());
    for (index, pane) in frame.panes.iter().enumerate() {
        let Some(rect) = pane.rect.within(width, height) else {
            continue;
        };
        let uniforms = FrameUniforms {
            frame: frame_index,
            viewport,
            grid: pane.cells.extent(),
            clear: pane.clear.to_f32(),
            background: BackgroundUniforms {
                row_offset: pane.cells.row_offset(),
                ..pane.background
            },
            text: pane
                .text
                .map_or_else(TextUniforms::default, |(_, text)| text),
        };
        placed.push(PlacedBlock {
            bytes: uniforms.to_bytes_with_hsb(pane.hsb),
            rect,
            pane: Some(index),
        });
    }
    for fill in frame.fills {
        let Some(rect) = fill.rect.within(width, height) else {
            continue;
        };
        // One cell the size of the rectangle, without a color of its own:
        // the background pass fills it with the clear color.
        let uniforms = FrameUniforms {
            frame: frame_index,
            viewport,
            grid: GridExtent::new(1, 1),
            clear: fill.color.to_f32(),
            background: BackgroundUniforms {
                cell_size: [rect.width as f32, rect.height as f32],
                grid_origin: [rect.x as f32, rect.y as f32],
                ..BackgroundUniforms::default()
            },
            text: TextUniforms::default(),
        };
        placed.push(PlacedBlock {
            bytes: uniforms.to_bytes(),
            rect,
            pane: None,
        });
    }
    placed
}

/// One color attachment that clears `texture` to `color` and stores it.
fn clear_pass(
    texture: &ProtocolObject<dyn MTLTexture>,
    color: ClearColor,
) -> Retained<MTLRenderPassDescriptor> {
    let pass = MTLRenderPassDescriptor::renderPassDescriptor();
    #[allow(unsafe_code)]
    // SAFETY: FFI-INDEX. Index 0 is below the eight color attachments every
    // Metal device exposes, and the array creates the descriptor on access.
    let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setTexture(Some(texture));
    attachment.setLoadAction(MTLLoadAction::Clear);
    attachment.setStoreAction(MTLStoreAction::Store);
    // The sRGB target takes the clear in linear light (ft-yccm0.4.7.3).
    let [red, green, blue, alpha] = color.to_linear();
    attachment.setClearColor(MTLClearColor {
        red,
        green,
        blue,
        alpha,
    });
    pass
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GpuFamily;

    fn device() -> MetalDevice {
        MetalDevice::system_default()
            .unwrap_or_else(|reason| panic!("this test needs an admitted Metal device: {reason}"))
    }

    #[test]
    fn gpu_family_raw_values_match_the_objc2_metal_constants() {
        assert_eq!(
            MTLGPUFamily(GpuFamily::Apple(1).raw()),
            MTLGPUFamily::Apple1
        );
        assert_eq!(
            MTLGPUFamily(GpuFamily::Apple(7).raw()),
            MTLGPUFamily::Apple7
        );
        assert_eq!(
            MTLGPUFamily(GpuFamily::Apple(10).raw()),
            MTLGPUFamily::Apple10
        );
        assert_eq!(MTLGPUFamily(GpuFamily::Mac2.raw()), MTLGPUFamily::Mac2);
        assert_eq!(MTLGPUFamily(GpuFamily::Metal3.raw()), MTLGPUFamily::Metal3);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn apple_silicon_default_device_is_admitted() {
        let device = device();
        let caps = device.capabilities();
        assert!(
            caps.apple_family
                .is_some_and(|family| family >= crate::MIN_APPLE_FAMILY),
            "{caps}"
        );
        assert!(caps.unified_memory, "{caps}");
        assert_ne!(caps.name, "");
    }

    /// ft-yccm0.4.7.3: the target is sRGB and clears take linear light, yet
    /// every opaque sRGB byte is stored exactly as configured.
    #[test]
    fn every_srgb_byte_survives_a_linear_clear_exactly() {
        let device = device();
        for byte in 0..=255_u8 {
            let unit = f32::from(byte) / 255.0;
            let color = ClearColor::from_srgba(unit, unit, 1.0 - unit, 1.0);
            let bytes = device
                .clear_offscreen(1, 1, color)
                .expect("offscreen clear");
            assert_eq!(bytes, [255 - byte, byte, byte, 255], "byte {byte}");
        }
    }

    #[test]
    fn offscreen_clear_reads_back_the_exact_color_in_every_pixel() {
        let device = device();
        // Odd extents catch row-pitch mistakes; two colors catch a stale or
        // zero-filled readback.
        for (width, height, rgba) in [
            (17_u32, 9_u32, [0x1e_u8, 0x2a, 0xc8, 0xff]),
            (3, 64, [0xff, 0x80, 0x00, 0xff]),
        ] {
            let [r, g, b, a] = rgba.map(|c| f32::from(c) / 255.0);
            let color = ClearColor::from_srgba(r, g, b, a);
            let expected = color.to_bgra8();
            assert_eq!(expected, [rgba[2], rgba[1], rgba[0], rgba[3]]);
            let bytes = device
                .clear_offscreen(width, height, color)
                .expect("offscreen clear");
            assert_eq!(
                bytes.len(),
                width as usize * height as usize * BYTES_PER_PIXEL
            );
            let (pixels, remainder) = bytes.as_chunks::<BYTES_PER_PIXEL>();
            assert_eq!(remainder, &[] as &[u8]);
            for (index, pixel) in pixels.iter().enumerate() {
                assert_eq!(*pixel, expected, "pixel {index} of {width}x{height}");
            }
        }
    }

    #[test]
    fn offscreen_clear_premultiplies_translucent_backgrounds() {
        let device = device();
        let color = ClearColor::from_srgba(1.0, 1.0, 1.0, 0.0);
        let bytes = device
            .clear_offscreen(4, 4, color)
            .expect("offscreen clear");
        assert!(bytes.iter().all(|&byte| byte == 0), "{bytes:?}");
    }

    #[test]
    fn offscreen_clear_rejects_zero_and_oversized_extents() {
        let device = device();
        let color = ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0);
        for (width, height) in [(0, 1), (1, 0), (MAX_TEXTURE_EXTENT + 1, 1), (1, u32::MAX)] {
            assert_eq!(
                device.clear_offscreen(width, height, color),
                Err(FrameError::InvalidExtent { width, height })
            );
        }
    }
}
