//! Frame slots on Metal (ft-yccm0.4.2.1).
//!
//! [`FrameSlots`] owns three slots of per-frame buffers in shared,
//! write-combined memory, paced by [`SlotRing`] and kept resident by one
//! `MTLResidencySet`. [`Submission`] encodes and commits a frame through
//! Metal 4 on macOS 26 (one command allocator, command buffer and argument
//! table per slot, created once and reused) or through Metal 3 (a command
//! buffer per frame, completion handlers) everywhere else. Both paths share
//! the buffer model and the pacing.

use crate::atlas::AtlasKind;
use crate::cell_bg::{BACKGROUND_SHADER, BackgroundUniforms, CellBgGrid};
use crate::cell_text::{CellTextGrid, TEXT_SHADER, TextUniforms, atlas_texture_index};
use crate::frame::{
    CELL_TEXT_INSTANCE_BYTES, FRAME_SLOTS, FrameUniforms, GridExtent, ROW_TABLE_BYTES_PER_ROW,
    SlotBuffer, SlotLease, SlotRing, SlotSizes, grown_capacity,
};
use crate::macos_atlas::GlyphAtlases;
use crate::uploads::{SlotUploads, UploadPlan, row_table_entry, text_region};
use crate::{ClearColor, FRAME_SLOT_TIMEOUT, FrameError, MAX_TEXTURE_EXTENT, SubmissionPath};
use block2::RcBlock;
use frankenterm_alloc::resource_ledger::{GpuBufferPurpose, GpuResourceGuard, GpuResourceLedger};
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::sel;
use objc2_foundation::{NSString, ns_string};
use objc2_metal::{
    MTL4ArgumentTable, MTL4ArgumentTableDescriptor, MTL4CommandAllocator, MTL4CommandBuffer,
    MTL4CommandEncoder, MTL4CommandQueue, MTL4CommitFeedback, MTL4CommitOptions,
    MTL4RenderCommandEncoder, MTL4RenderPassDescriptor, MTLBlendFactor, MTLBlitCommandEncoder,
    MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLDevice, MTLDrawable, MTLGPUFamily, MTLLibrary, MTLLoadAction, MTLOrigin,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder,
    MTLRenderPassColorAttachmentDescriptor, MTLRenderPassColorAttachmentDescriptorArray,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLRenderStages,
    MTLResidencySet, MTLResidencySetDescriptor, MTLResource, MTLResourceOptions, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::CAMetalLayer;
use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Storage of every per-frame buffer: CPU-visible unified memory that the
/// CPU only writes, so write-combined (uncached CPU reads are slow, writes
/// stream straight to memory).
fn slot_buffer_options() -> MTLResourceOptions {
    MTLResourceOptions::StorageModeShared | MTLResourceOptions::CPUCacheModeWriteCombined
}

/// How a slot buffer is reported in the GPU resource ledger (ft-yccm0.1.7):
/// uniforms as uniforms, the per-instance and per-row buffers as vertex data.
fn ledger_purpose(kind: SlotBuffer) -> GpuBufferPurpose {
    match kind {
        SlotBuffer::Uniforms => GpuBufferPurpose::Uniform,
        SlotBuffer::CellBg | SlotBuffer::CellText | SlotBuffer::RowTable => {
            GpuBufferPurpose::Vertex
        }
    }
}

fn sizes_for(grid: GridExtent) -> Result<SlotSizes, FrameError> {
    SlotSizes::for_grid(grid).ok_or(FrameError::GridTooLarge {
        rows: grid.rows,
        cols: grid.cols,
    })
}

/// One buffer and its ledger entry, released together.
struct SlotAllocation {
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    _ledger: GpuResourceGuard,
}

struct Slot {
    /// Indexed by [`SlotBuffer::index`].
    buffers: Vec<SlotAllocation>,
    /// Bumped whenever one of the slot's buffers is reallocated, so bindings
    /// made from the old addresses are refreshed.
    generation: u64,
    /// What the slot's buffers hold, so a frame uploads only the rows that
    /// changed since (ft-yccm0.4.2.4).
    uploads: SlotUploads,
}

/// The renderer's frame slots; see the module docs.
pub(crate) struct FrameSlots {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    ring: Arc<SlotRing>,
    slots: Vec<Slot>,
    residency: Option<Retained<ProtocolObject<dyn MTLResidencySet>>>,
    ledger: &'static GpuResourceLedger,
    allocations: u64,
    /// Bytes the last frame uploaded into its slot, and since creation.
    upload_bytes_last: u64,
    upload_bytes_total: u64,
}

impl FrameSlots {
    /// Three slots sized for `grid`, every buffer added to one residency set
    /// when the OS has them (macOS 15+).
    pub(crate) fn new(
        device: Retained<ProtocolObject<dyn MTLDevice>>,
        grid: GridExtent,
        ledger: &'static GpuResourceLedger,
    ) -> Result<Self, FrameError> {
        let sizes = sizes_for(grid)?;
        let residency = new_residency_set(&device, ns_string!("frankenterm frame slots"));
        let mut this = Self {
            device,
            ring: SlotRing::new(),
            slots: Vec::with_capacity(FRAME_SLOTS),
            residency,
            ledger,
            allocations: 0,
            upload_bytes_last: 0,
            upload_bytes_total: 0,
        };
        for index in 0..FRAME_SLOTS {
            let mut buffers = Vec::with_capacity(SlotBuffer::ALL.len());
            for kind in SlotBuffer::ALL {
                buffers.push(this.allocate(kind, grown_capacity(0, sizes.bytes(kind)), index)?);
            }
            this.slots.push(Slot {
                buffers,
                generation: 0,
                uploads: SlotUploads::default(),
            });
        }
        if let Some(set) = &this.residency {
            for slot in &this.slots {
                for allocation in &slot.buffers {
                    set.addAllocation(ProtocolObject::from_ref(&*allocation.buffer));
                }
            }
            set.commit();
            set.requestResidency();
        }
        Ok(this)
    }

    fn allocate(
        &mut self,
        kind: SlotBuffer,
        bytes: usize,
        slot: usize,
    ) -> Result<SlotAllocation, FrameError> {
        let buffer = self
            .device
            .newBufferWithLength_options(bytes, slot_buffer_options())
            .ok_or(FrameError::AllocationFailed {
                what: kind.as_str(),
                bytes,
            })?;
        buffer.setLabel(Some(&NSString::from_str(&format!(
            "frankenterm slot {slot} {}",
            kind.as_str()
        ))));
        let ledger = self.ledger.track_buffer(
            ledger_purpose(kind),
            u64::try_from(buffer.length()).unwrap_or(u64::MAX),
        );
        self.allocations += 1;
        Ok(SlotAllocation {
            buffer,
            _ledger: ledger,
        })
    }

    /// [`Self::begin_frame`] for a frame without glyph instances.
    #[cfg(test)]
    pub(crate) fn begin(
        &mut self,
        grid: GridExtent,
        timeout: Duration,
    ) -> Result<SlotLease, FrameError> {
        self.begin_frame(grid, 0, timeout)
    }

    /// Leases the next slot (waiting up to `timeout` for the GPU to finish
    /// the frame that last used it) and fits its buffers to `grid` and
    /// `text_instances` glyph instances, which may need a larger CellText
    /// buffer than the grid reserves.
    pub(crate) fn begin_frame(
        &mut self,
        grid: GridExtent,
        text_instances: usize,
        timeout: Duration,
    ) -> Result<SlotLease, FrameError> {
        let sizes = SlotSizes::for_frame(grid, text_instances).ok_or(FrameError::GridTooLarge {
            rows: grid.rows,
            cols: grid.cols,
        })?;
        let lease = self
            .ring
            .acquire(timeout)
            .map_err(|timeout| FrameError::FrameSlotTimeout {
                slot: timeout.slot,
                in_flight: timeout.in_flight,
            })?;
        self.fit_slot(lease.slot(), &sizes)?;
        Ok(lease)
    }

    /// Grows the leased `slot`'s buffers that are too small for `sizes`.
    /// The slot is leased, so no in-flight frame uses the buffers replaced.
    fn fit_slot(&mut self, slot: usize, sizes: &SlotSizes) -> Result<(), FrameError> {
        let mut grown = false;
        for kind in SlotBuffer::ALL {
            let current = self.slots[slot].buffers[kind.index()].buffer.length();
            let capacity = grown_capacity(current, sizes.bytes(kind));
            if capacity == current {
                continue;
            }
            let replacement = self.allocate(kind, capacity, slot)?;
            if let Some(set) = &self.residency {
                set.addAllocation(ProtocolObject::from_ref(&*replacement.buffer));
            }
            let old = std::mem::replace(&mut self.slots[slot].buffers[kind.index()], replacement);
            if let Some(set) = &self.residency {
                set.removeAllocation(ProtocolObject::from_ref(&*old.buffer));
            }
            grown = true;
        }
        if grown {
            self.slots[slot].generation += 1;
            if let Some(set) = &self.residency {
                set.commit();
            }
        }
        Ok(())
    }

    /// Copies `bytes` into the leased slot's `kind` buffer at `offset`.
    pub(crate) fn write(
        &self,
        lease: &SlotLease,
        kind: SlotBuffer,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), FrameError> {
        assert!(
            lease.is_from(&self.ring),
            "a slot lease from another renderer's ring"
        );
        let buffer = self.buffer(lease.slot(), kind);
        let capacity = buffer.length();
        if offset
            .checked_add(bytes.len())
            .is_none_or(|end| end > capacity)
        {
            return Err(FrameError::SlotWriteOutOfBounds {
                buffer: kind.as_str(),
                offset,
                len: bytes.len(),
                capacity,
            });
        }
        let destination = buffer.contents().cast::<u8>().as_ptr().wrapping_add(offset);
        #[allow(unsafe_code)]
        // SAFETY: BUFFER-WRITE. `lease` comes from this ring (asserted), so
        // its slot is not in flight: the GPU finished the last frame that read
        // this buffer before the lease was granted. The buffer is shared
        // (CPU-visible) storage of `capacity` bytes, and offset + len was
        // checked to be within it. `bytes` is ordinary Rust memory, which
        // cannot overlap the Metal allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
        }
        Ok(())
    }

    /// Uploads into the leased slot what `cells` and `text` changed since
    /// the slot last held them (ft-yccm0.4.2.4): only their dirty ring rows,
    /// or everything when the slot is laid out afresh. Fits the slot's
    /// buffers for the glyph regions first. Returns how many instances the
    /// text draw covers (`rows * capacity`; zero without text).
    pub(crate) fn upload(
        &mut self,
        lease: &SlotLease,
        grid: GridExtent,
        cells: Option<&CellBgGrid>,
        text: Option<&CellTextGrid>,
    ) -> Result<usize, FrameError> {
        assert!(
            lease.is_from(&self.ring),
            "a slot lease from another renderer's ring"
        );
        let slot = lease.slot();
        let generation = self.slots[slot].generation;
        let mut plan = self.slots[slot].uploads.plan(cells, text, generation);
        let sizes =
            SlotSizes::for_frame(grid, plan.instances(grid)).ok_or(FrameError::GridTooLarge {
                rows: grid.rows,
                cols: grid.cols,
            })?;
        self.fit_slot(slot, &sizes)?;
        let fitted = self.slots[slot].generation;
        if fitted != generation {
            // The buffers were replaced: they hold nothing yet.
            self.slots[slot].uploads.invalidate();
            plan = self.slots[slot].uploads.plan(cells, text, fitted);
        }
        if let Err(err) = self.write_plan(lease, &plan, cells, text) {
            self.slots[slot].uploads.invalidate();
            return Err(err);
        }
        let bytes = plan.bytes(grid);
        self.upload_bytes_last = bytes;
        self.upload_bytes_total = self.upload_bytes_total.saturating_add(bytes);
        Ok(text.map_or(0, |_| plan.instances(grid)))
    }

    /// Copies `plan`'s rows into the leased slot: cell background rows at
    /// their ring offsets, glyph regions zero-padded to the plan's capacity,
    /// and their row-table entries.
    fn write_plan(
        &self,
        lease: &SlotLease,
        plan: &UploadPlan,
        cells: Option<&CellBgGrid>,
        text: Option<&CellTextGrid>,
    ) -> Result<(), FrameError> {
        if let Some(cells) = cells {
            let row = cells.extent().cols as usize * 4;
            for &ring in &plan.cell_rows {
                let ring = ring as usize;
                self.write(
                    lease,
                    SlotBuffer::CellBg,
                    ring * row,
                    cells.ring_row_bytes(ring),
                )?;
            }
        }
        if let Some(text) = text {
            let region = plan.capacity as usize * CELL_TEXT_INSTANCE_BYTES;
            let mut bytes = Vec::with_capacity(region);
            for &ring in &plan.text_rows {
                let ring = ring as usize;
                bytes.clear();
                text_region(text, ring, plan.capacity, &mut bytes);
                self.write(lease, SlotBuffer::CellText, ring * region, &bytes)?;
                self.write(
                    lease,
                    SlotBuffer::RowTable,
                    ring * ROW_TABLE_BYTES_PER_ROW,
                    &row_table_entry(text, ring, plan.capacity),
                )?;
            }
        }
        Ok(())
    }

    /// Bytes the last frame uploaded, and all frames since creation.
    pub(crate) fn upload_bytes(&self) -> (u64, u64) {
        (self.upload_bytes_last, self.upload_bytes_total)
    }

    pub(crate) fn buffer(&self, slot: usize, kind: SlotBuffer) -> &ProtocolObject<dyn MTLBuffer> {
        &self.slots[slot].buffers[kind.index()].buffer
    }

    pub(crate) fn generation(&self, slot: usize) -> u64 {
        self.slots[slot].generation
    }

    pub(crate) fn residency(&self) -> Option<&ProtocolObject<dyn MTLResidencySet>> {
        self.residency.as_deref()
    }

    pub(crate) fn ring(&self) -> &Arc<SlotRing> {
        &self.ring
    }

    /// Buffers created since construction, including the initial twelve.
    pub(crate) fn allocations(&self) -> u64 {
        self.allocations
    }

    /// Renders `request.cells` with the background pass, and `request.text`
    /// with the text pass when given, into an offscreen texture through
    /// `submission` and reads it back through `readback` (a Metal 3 queue):
    /// BGRA8, row-major, tightly packed. Waits for the GPU; the render
    /// snapshot (ft-yccm0.1.10) uses it, nothing is presented.
    pub(crate) fn render_cells_offscreen(
        &mut self,
        submission: &Submission,
        pipeline: &BackgroundPipeline,
        readback: &ProtocolObject<dyn MTLCommandQueue>,
        request: &OffscreenCells<'_>,
    ) -> Result<Vec<u8>, FrameError> {
        let (width, height) = (request.width, request.height);
        if !(1..=MAX_TEXTURE_EXTENT).contains(&width) || !(1..=MAX_TEXTURE_EXTENT).contains(&height)
        {
            return Err(FrameError::InvalidExtent { width, height });
        }
        let (width_px, height_px) = (width as usize, height as usize);
        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. BGRA8Unorm is color-renderable and the extent
        // was checked to be within 1..=MAX_TEXTURE_EXTENT on both axes above.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width_px,
                height_px,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        descriptor.setStorageMode(MTLStorageMode::Private);
        let target = self.device.newTextureWithDescriptor(&descriptor).ok_or(
            FrameError::AllocationFailed {
                what: "offscreen render target",
                bytes: width_px * height_px * 4,
            },
        )?;
        // Metal 4 makes nothing resident implicitly.
        if let Some(set) = &self.residency {
            set.addAllocation(ProtocolObject::from_ref(&*target));
            set.commit();
        }
        let result = self.render_offscreen_into(submission, pipeline, readback, request, &target);
        if let Some(set) = &self.residency {
            set.removeAllocation(ProtocolObject::from_ref(&*target));
            set.commit();
        }
        result
    }

    fn render_offscreen_into(
        &mut self,
        submission: &Submission,
        pipeline: &BackgroundPipeline,
        readback: &ProtocolObject<dyn MTLCommandQueue>,
        request: &OffscreenCells<'_>,
        target: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<Vec<u8>, FrameError> {
        let cells = request.cells;
        let failed_before = submission.failed_frames();
        let lease = self.begin_frame(cells.extent(), 0, FRAME_SLOT_TIMEOUT)?;
        let instances = self.upload(
            &lease,
            cells.extent(),
            Some(cells),
            request.text.as_ref().map(|text| text.grid),
        )?;
        let uniforms = FrameUniforms {
            frame: lease.frame(),
            viewport: [request.width, request.height],
            grid: cells.extent(),
            clear: request.clear.to_f32(),
            background: BackgroundUniforms {
                row_offset: cells.row_offset(),
                ..request.background
            },
            text: request
                .text
                .as_ref()
                .map_or_else(TextUniforms::default, |text| text.uniforms),
        };
        self.write(&lease, SlotBuffer::Uniforms, 0, &uniforms.to_bytes())?;
        let text = request.text.as_ref().map(|text| TextDraw {
            pipeline: text.pipeline,
            atlases: text.atlases,
            instances,
        });
        submission.encode_frame(
            self,
            lease,
            target,
            None,
            request.clear,
            Some(pipeline),
            text,
        )?;
        if !self.ring.wait_idle(OFFSCREEN_TIMEOUT) {
            return Err(FrameError::CommandFailed {
                detail: "the offscreen frame did not finish".to_string(),
            });
        }
        if submission.failed_frames() != failed_before {
            return Err(FrameError::CommandFailed {
                detail: "the offscreen frame finished in error".to_string(),
            });
        }
        read_texture_bgra(
            &self.device,
            readback,
            target,
            request.width,
            request.height,
        )
    }

    /// Reads back `len` bytes of a slot buffer the GPU has finished with.
    #[cfg(test)]
    #[allow(unsafe_code)]
    pub(crate) fn read(&self, slot: usize, kind: SlotBuffer, len: usize) -> Vec<u8> {
        assert_eq!(
            self.ring.state(slot),
            crate::frame::SlotState::Free,
            "only a slot the GPU has finished may be read"
        );
        let buffer = self.buffer(slot, kind);
        assert!(len <= buffer.length());
        let contents = buffer.contents().cast::<u8>();
        // SAFETY: BUFFER-CONTENTS. The slot is free, so no GPU work writes it;
        // `len` was checked against `-length`; the bytes are copied into an
        // owned Vec while the buffer is alive.
        unsafe { std::slice::from_raw_parts(contents.as_ptr(), len) }.to_vec()
    }
}

/// Longest an offscreen render waits for the GPU.
const OFFSCREEN_TIMEOUT: Duration = Duration::from_secs(10);

/// One offscreen render: the background pass, then the text pass if `text`
/// is given.
pub(crate) struct OffscreenCells<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) cells: &'a CellBgGrid,
    pub(crate) clear: ClearColor,
    pub(crate) background: BackgroundUniforms,
    pub(crate) text: Option<OffscreenText<'a>>,
}

/// The text pass of an offscreen render. `grid` must have the cells'
/// extent and ring offset (checked by the caller), and the atlases'
/// residency set must have joined the submission's queue
/// ([`Submission::add_residency_set`]).
pub(crate) struct OffscreenText<'a> {
    pub(crate) grid: &'a CellTextGrid,
    pub(crate) uniforms: TextUniforms,
    pub(crate) pipeline: &'a TextPipeline,
    pub(crate) atlases: &'a GlyphAtlases,
}

/// Copies a BGRA8 texture into shared memory with a Metal 3 blit and
/// returns its bytes: row-major, tightly packed.
#[allow(unsafe_code)]
pub(crate) fn read_texture_bgra(
    device: &ProtocolObject<dyn MTLDevice>,
    queue: &ProtocolObject<dyn MTLCommandQueue>,
    texture: &ProtocolObject<dyn MTLTexture>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, FrameError> {
    let (width_px, height_px) = (width as usize, height as usize);
    let bytes_per_row = width_px * 4;
    let len = bytes_per_row * height_px;
    let buffer = device
        .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared)
        .ok_or(FrameError::AllocationFailed {
            what: "readback buffer",
            bytes: len,
        })?;
    let commands = queue
        .commandBuffer()
        .ok_or(FrameError::CommandBufferUnavailable)?;
    let blit = commands
        .blitCommandEncoder()
        .ok_or(FrameError::EncoderUnavailable)?;
    // SAFETY: FFI-EXTENT. The source region is the texture's whole width x
    // height x 1 extent in slice 0, level 0 (the caller's texture has exactly
    // this extent and the 4-byte BGRA8 format). The destination pitch is
    // width * 4 bytes and `buffer` holds exactly pitch * height bytes.
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture,
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
    if commands.status() != MTLCommandBufferStatus::Completed {
        return Err(FrameError::CommandFailed {
            detail: format!("readback status {}", commands.status().0),
        });
    }
    if buffer.length() < len {
        return Err(FrameError::AllocationFailed {
            what: "readback buffer",
            bytes: len,
        });
    }
    let contents = buffer.contents().cast::<u8>();
    // SAFETY: BUFFER-CONTENTS. The blit that wrote `buffer` completed
    // successfully, `-length` was checked to be at least `len`, and the bytes
    // are copied into an owned Vec while `buffer` is alive.
    let bytes = unsafe { std::slice::from_raw_parts(contents.as_ptr(), len) }.to_vec();
    Ok(bytes)
}

/// A residency set, or `None` before macOS 15 (no `MTLResidencySet`).
fn new_residency_set(
    device: &ProtocolObject<dyn MTLDevice>,
    label: &NSString,
) -> Option<Retained<ProtocolObject<dyn MTLResidencySet>>> {
    if !device.respondsToSelector(sel!(newResidencySetWithDescriptor:error:)) {
        return None;
    }
    let descriptor = MTLResidencySetDescriptor::new();
    descriptor.setLabel(Some(label));
    device.newResidencySetWithDescriptor_error(&descriptor).ok()
}

/// Whether `device` and the OS run Metal 4 command queues: macOS 26 with an
/// `MTLGPUFamilyMetal4` GPU.
pub(crate) fn supports_metal4(device: &ProtocolObject<dyn MTLDevice>) -> bool {
    device.respondsToSelector(sel!(newMTL4CommandQueue))
        && device.supportsFamily(MTLGPUFamily::Metal4)
}

/// Color attachment 0 of a render pass descriptor.
#[allow(unsafe_code)]
fn color_attachment(
    attachments: &MTLRenderPassColorAttachmentDescriptorArray,
) -> Retained<MTLRenderPassColorAttachmentDescriptor> {
    // SAFETY: FFI-INDEX. Index 0 is below the eight color attachments every
    // Metal device exposes, and the array creates the descriptor on access.
    unsafe { attachments.objectAtIndexedSubscript(0) }
}

fn configure_clear(
    attachment: &MTLRenderPassColorAttachmentDescriptor,
    target: &ProtocolObject<dyn MTLTexture>,
    color: ClearColor,
) {
    attachment.setTexture(Some(target));
    attachment.setLoadAction(MTLLoadAction::Clear);
    attachment.setStoreAction(MTLStoreAction::Store);
    attachment.setClearColor(MTLClearColor {
        red: color.red,
        green: color.green,
        blue: color.blue,
        alpha: color.alpha,
    });
}

/// How frames reach the GPU; see the module docs.
pub(crate) enum Submission {
    Metal3(Metal3Submission),
    Metal4(Metal4Submission),
}

pub(crate) struct Metal3Submission {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// One render pass descriptor per slot, reused every frame.
    passes: Vec<Retained<MTLRenderPassDescriptor>>,
    failed: Arc<AtomicU64>,
}

pub(crate) struct Metal4Submission {
    queue: Retained<ProtocolObject<dyn MTL4CommandQueue>>,
    allocators: Vec<Retained<ProtocolObject<dyn MTL4CommandAllocator>>>,
    command_buffers: Vec<Retained<ProtocolObject<dyn MTL4CommandBuffer>>>,
    argument_tables: Vec<Retained<ProtocolObject<dyn MTL4ArgumentTable>>>,
    /// The slot generation each argument table was last bound for.
    bound: Vec<Cell<Option<u64>>>,
    passes: Vec<Retained<MTL4RenderPassDescriptor>>,
    failed: Arc<AtomicU64>,
}

impl Submission {
    /// The Metal 3 path on `queue`. The frame slots' residency set joins the
    /// queue when the OS supports residency sets.
    pub(crate) fn metal3(
        queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
        frames: &FrameSlots,
    ) -> Self {
        if let Some(set) = frames.residency()
            && queue.respondsToSelector(sel!(addResidencySet:))
        {
            queue.addResidencySet(set);
        }
        Self::Metal3(Metal3Submission {
            queue,
            passes: (0..FRAME_SLOTS)
                .map(|_| MTLRenderPassDescriptor::renderPassDescriptor())
                .collect(),
            failed: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The Metal 4 path: a new `MTL4CommandQueue` with the frame slots'
    /// residency set, and one command allocator, command buffer and argument
    /// table per slot. Requires [`supports_metal4`] and a residency set.
    pub(crate) fn metal4(
        device: &ProtocolObject<dyn MTLDevice>,
        frames: &FrameSlots,
    ) -> Result<Self, String> {
        if !supports_metal4(device) {
            return Err("the device or OS has no Metal 4 command queues".to_string());
        }
        let residency = frames
            .residency()
            .ok_or_else(|| "no residency set for the frame slots".to_string())?;
        let queue = device
            .newMTL4CommandQueue()
            .ok_or_else(|| "newMTL4CommandQueue returned nil".to_string())?;
        queue.addResidencySet(residency);
        let mut allocators = Vec::with_capacity(FRAME_SLOTS);
        let mut command_buffers = Vec::with_capacity(FRAME_SLOTS);
        let mut argument_tables = Vec::with_capacity(FRAME_SLOTS);
        let table_descriptor = MTL4ArgumentTableDescriptor::new();
        table_descriptor.setMaxBufferBindCount(SlotBuffer::ALL.len());
        // The text pass's grayscale and color atlases (ft-yccm0.4.2.3).
        table_descriptor.setMaxTextureBindCount(AtlasKind::ALL.len());
        for _ in 0..FRAME_SLOTS {
            allocators.push(
                device
                    .newCommandAllocator()
                    .ok_or_else(|| "newCommandAllocator returned nil".to_string())?,
            );
            command_buffers.push(
                device
                    .newCommandBuffer()
                    .ok_or_else(|| "newCommandBuffer returned nil".to_string())?,
            );
            argument_tables.push(
                device
                    .newArgumentTableWithDescriptor_error(&table_descriptor)
                    .map_err(|error| {
                        format!("newArgumentTable failed: {}", error.localizedDescription())
                    })?,
            );
        }
        Ok(Self::Metal4(Metal4Submission {
            queue,
            allocators,
            command_buffers,
            argument_tables,
            bound: (0..FRAME_SLOTS).map(|_| Cell::new(None)).collect(),
            passes: (0..FRAME_SLOTS)
                .map(|_| MTL4RenderPassDescriptor::new())
                .collect(),
            failed: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub(crate) fn path(&self) -> SubmissionPath {
        match self {
            Self::Metal3(_) => SubmissionPath::Metal3,
            Self::Metal4(_) => SubmissionPath::Metal4,
        }
    }

    /// Makes the layer's drawables resident. Metal 4 queues track no
    /// residency implicitly, so the layer's own residency set joins the
    /// queue once; Metal 3 queues need nothing.
    pub(crate) fn add_layer(&self, layer: &CAMetalLayer) {
        if let Self::Metal4(metal4) = self {
            metal4.queue.addResidencySet(&layer.residencySet());
        }
    }

    /// Adds `set` (the glyph atlases') to the queue's resident sets, once per
    /// set. Metal 4 needs it before a frame samples the atlases; Metal 3
    /// makes bound textures resident itself and takes it only as a hint.
    pub(crate) fn add_residency_set(&self, set: &ProtocolObject<dyn MTLResidencySet>) {
        match self {
            Self::Metal3(metal3) => {
                if metal3.queue.respondsToSelector(sel!(addResidencySet:)) {
                    metal3.queue.addResidencySet(set);
                }
            }
            Self::Metal4(metal4) => metal4.queue.addResidencySet(set),
        }
    }

    /// Frames whose command buffer finished in error.
    pub(crate) fn failed_frames(&self) -> u64 {
        match self {
            Self::Metal3(metal3) => metal3.failed.load(Ordering::Relaxed),
            Self::Metal4(metal4) => metal4.failed.load(Ordering::Relaxed),
        }
    }

    /// Encodes one frame into the leased slot and commits it: a render pass
    /// that clears `target` to `color`; with a `background` pipeline, draws
    /// the background pass from the slot's uniforms and CellBg buffer; with
    /// `text`, then draws the slot's CellText instances over it in one
    /// instanced draw. Presents `drawable` when given. The slot stays in
    /// flight until the GPU finishes the frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_frame(
        &self,
        frames: &FrameSlots,
        lease: SlotLease,
        target: &ProtocolObject<dyn MTLTexture>,
        drawable: Option<&ProtocolObject<dyn MTLDrawable>>,
        color: ClearColor,
        background: Option<&BackgroundPipeline>,
        text: Option<TextDraw<'_>>,
    ) -> Result<(), FrameError> {
        assert!(
            lease.is_from(frames.ring()),
            "a slot lease from another renderer's ring"
        );
        let frame = EncodedFrame {
            target,
            drawable,
            color,
            background,
            text: text.filter(|text| text.instances > 0),
        };
        match self {
            Self::Metal3(metal3) => metal3.encode_frame(frames, lease, &frame),
            Self::Metal4(metal4) => metal4.encode_frame(frames, lease, &frame),
        }
    }
}

/// What one frame draws, shared by both submission paths.
struct EncodedFrame<'a> {
    target: &'a ProtocolObject<dyn MTLTexture>,
    drawable: Option<&'a ProtocolObject<dyn MTLDrawable>>,
    color: ClearColor,
    background: Option<&'a BackgroundPipeline>,
    /// Present only with at least one instance.
    text: Option<TextDraw<'a>>,
}

/// The text pass of one frame (ft-yccm0.4.2.3): the pipeline, the atlases
/// its glyphs sample, and how many CellText instances the draw covers:
/// every ring row's region, `rows * capacity`, whose unused instances are
/// empty quads (ft-yccm0.4.2.4, [`FrameSlots::upload`]).
#[derive(Clone, Copy)]
pub(crate) struct TextDraw<'a> {
    pub(crate) pipeline: &'a TextPipeline,
    pub(crate) atlases: &'a GlyphAtlases,
    pub(crate) instances: usize,
}

/// A render pipeline drawing into BGRA8Unorm, compiled from `source`;
/// `blended` enables premultiplied source-over onto what is already there.
fn render_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    pass: &str,
    source: &str,
    vertex: &NSString,
    fragment: &NSString,
    blended: bool,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let library = device
        .newLibraryWithSource_options_error(&NSString::from_str(source), None)
        .map_err(|error| {
            format!(
                "the {pass} shader did not compile: {}",
                error.localizedDescription()
            )
        })?;
    let vertex = library
        .newFunctionWithName(vertex)
        .ok_or_else(|| format!("the {pass} shader has no {vertex}"))?;
    let fragment = library
        .newFunctionWithName(fragment)
        .ok_or_else(|| format!("the {pass} shader has no {fragment}"))?;
    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setLabel(Some(&NSString::from_str(&format!(
        "frankenterm {pass} pass"
    ))));
    descriptor.setVertexFunction(Some(&vertex));
    descriptor.setFragmentFunction(Some(&fragment));
    #[allow(unsafe_code)]
    // SAFETY: FFI-INDEX. Index 0 is below the eight color attachments every
    // Metal device exposes, and the array creates the descriptor on access.
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    if blended {
        attachment.setBlendingEnabled(true);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }
    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|error| {
            format!(
                "the {pass} pipeline could not be created: {}",
                error.localizedDescription()
            )
        })
}

/// The background pass's render pipeline (ft-yccm0.4.2.2), compiled from
/// [`BACKGROUND_SHADER`] once per renderer.
pub(crate) struct BackgroundPipeline {
    state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl BackgroundPipeline {
    pub(crate) fn new(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self, String> {
        let state = render_pipeline(
            device,
            "background",
            BACKGROUND_SHADER,
            ns_string!("bg_vertex"),
            ns_string!("bg_fragment"),
            false,
        )?;
        Ok(Self { state })
    }
}

/// The text pass's render pipeline (ft-yccm0.4.2.3), compiled from
/// [`TEXT_SHADER`] once per renderer: one quad per CellText instance,
/// premultiplied source-over onto the background pass.
pub(crate) struct TextPipeline {
    state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl TextPipeline {
    pub(crate) fn new(device: &ProtocolObject<dyn MTLDevice>) -> Result<Self, String> {
        let state = render_pipeline(
            device,
            "text",
            TEXT_SHADER,
            ns_string!("text_vertex"),
            ns_string!("text_fragment"),
            true,
        )?;
        Ok(Self { state })
    }
}

impl Metal3Submission {
    fn encode_frame(
        &self,
        frames: &FrameSlots,
        lease: SlotLease,
        frame: &EncodedFrame<'_>,
    ) -> Result<(), FrameError> {
        let slot = lease.slot();
        let commands = self
            .queue
            .commandBuffer()
            .ok_or(FrameError::CommandBufferUnavailable)?;
        let pass = &self.passes[slot];
        let attachment = color_attachment(&pass.colorAttachments());
        configure_clear(&attachment, frame.target, frame.color);
        let encoder = commands.renderCommandEncoderWithDescriptor(pass);
        // The descriptor outlives the frame; it must not keep the drawable's
        // texture alive.
        attachment.setTexture(None);
        let encoder = encoder.ok_or(FrameError::EncoderUnavailable)?;
        if let Some(background) = frame.background {
            encoder.setRenderPipelineState(&background.state);
            for kind in [SlotBuffer::Uniforms, SlotBuffer::CellBg] {
                #[allow(unsafe_code)]
                // SAFETY: FFI-DRAW. The buffer is a live slot buffer of the
                // leased slot, bound from offset 0 at its SlotBuffer index,
                // which is the shader's [[buffer(n)]] and below Metal's 31
                // buffer slots.
                unsafe {
                    encoder.setFragmentBuffer_offset_atIndex(
                        Some(frames.buffer(slot, kind)),
                        0,
                        kind.index(),
                    );
                }
            }
            #[allow(unsafe_code)]
            // SAFETY: FFI-DRAW. Three vertices of one triangle; bg_vertex
            // derives positions from vertex_id and reads no vertex buffer.
            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
        }
        if let Some(text) = frame.text {
            encoder.setRenderPipelineState(&text.pipeline.state);
            for kind in [SlotBuffer::Uniforms, SlotBuffer::CellText] {
                #[allow(unsafe_code)]
                // SAFETY: FFI-DRAW. The buffer is a live slot buffer of the
                // leased slot, bound from offset 0 at its SlotBuffer index,
                // which is the shader's [[buffer(n)]] and below Metal's 31
                // buffer slots.
                unsafe {
                    encoder.setVertexBuffer_offset_atIndex(
                        Some(frames.buffer(slot, kind)),
                        0,
                        kind.index(),
                    );
                }
            }
            #[allow(unsafe_code)]
            // SAFETY: FFI-DRAW. The uniforms buffer of the leased slot, bound
            // from offset 0 at the shader's [[buffer(0)]].
            unsafe {
                encoder.setFragmentBuffer_offset_atIndex(
                    Some(frames.buffer(slot, SlotBuffer::Uniforms)),
                    0,
                    SlotBuffer::Uniforms.index(),
                );
            }
            for kind in AtlasKind::ALL {
                #[allow(unsafe_code)]
                // SAFETY: FFI-DRAW. A live atlas texture that the atlases
                // keep (and retire only after the frames that may sample it
                // finish), bound at the shader's [[texture(n)]], below
                // Metal's 31 texture slots. Metal 3 retains bound textures.
                unsafe {
                    encoder.setFragmentTexture_atIndex(
                        Some(text.atlases.texture(kind)),
                        atlas_texture_index(kind),
                    );
                }
            }
            #[allow(unsafe_code)]
            // SAFETY: FFI-DRAW. Four strip vertices per instance, positions
            // derived from vertex_id; text_vertex reads CellText only at
            // instance_id < instances, all of which the frame wrote into a
            // buffer fitted for them.
            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                    text.instances,
                );
            }
        }
        encoder.endEncoding();
        if let Some(drawable) = frame.drawable {
            commands.presentDrawable(drawable);
        }
        let token = lease.submit();
        let failed = Arc::clone(&self.failed);
        let handler = RcBlock::new(
            move |commands: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                #[allow(unsafe_code)]
                // SAFETY: FFI-BLOCK. Metal passes the command buffer that
                // completed, valid for the duration of the handler.
                let status = unsafe { commands.as_ref() }.status();
                if status != MTLCommandBufferStatus::Completed {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
                token.complete();
            },
        );
        #[allow(unsafe_code)]
        // SAFETY: FFI-BLOCK. Metal copies the block, so the local `handler`
        // may drop after this call. The closure owns only a CompletionToken
        // and an Arc<AtomicU64>, both Send + Sync and 'static, because Metal
        // runs it on a thread of its own choosing. It is added before commit,
        // as Metal requires.
        unsafe {
            commands.addCompletedHandler(RcBlock::as_ptr(&handler));
        }
        commands.commit();
        Ok(())
    }
}

impl Metal4Submission {
    /// Points the slot's argument table at its buffers, if they changed since
    /// the last bind. Steady state binds nothing and creates nothing.
    fn bind_slot(&self, frames: &FrameSlots, slot: usize) {
        let generation = frames.generation(slot);
        if self.bound[slot].get() == Some(generation) {
            return;
        }
        let table = &self.argument_tables[slot];
        for kind in SlotBuffer::ALL {
            let address = frames.buffer(slot, kind).gpuAddress();
            #[allow(unsafe_code)]
            // SAFETY: FFI-ARGTABLE. `address` is the GPU address of a live
            // buffer that the frame slots own and keep in the queue's
            // residency set; the binding index is below the table's
            // maxBufferBindCount of SlotBuffer::ALL.len(). The slot's
            // generation changes whenever a buffer is replaced, so a stale
            // address is rebound before the next frame on this slot.
            unsafe {
                table.setAddress_atIndex(address, kind.index());
            }
        }
        self.bound[slot].set(Some(generation));
    }

    fn encode_frame(
        &self,
        frames: &FrameSlots,
        lease: SlotLease,
        frame: &EncodedFrame<'_>,
    ) -> Result<(), FrameError> {
        let (target, drawable, color) = (frame.target, frame.drawable, frame.color);
        let slot = lease.slot();
        self.bind_slot(frames, slot);
        if let Some(text) = frame.text {
            // Every frame: growth replaces an atlas texture.
            for kind in AtlasKind::ALL {
                #[allow(unsafe_code)]
                // SAFETY: FFI-ARGTABLE. The resource ID is a live atlas
                // texture's; the atlases keep it in their residency set, which
                // joined this queue, until the frames that may sample it have
                // finished. The index is below the table's
                // maxTextureBindCount of AtlasKind::ALL.len().
                unsafe {
                    self.argument_tables[slot].setTexture_atIndex(
                        text.atlases.texture(kind).gpuResourceID(),
                        atlas_texture_index(kind),
                    );
                }
            }
        }
        let commands = &self.command_buffers[slot];
        // The lease proves this slot's previous frame completed, so its
        // allocator's memory is free to reuse.
        self.allocators[slot].reset();
        commands.beginCommandBufferWithAllocator(&self.allocators[slot]);
        let pass = &self.passes[slot];
        let attachment = color_attachment(&pass.colorAttachments());
        configure_clear(&attachment, target, color);
        let encoder = commands.renderCommandEncoderWithDescriptor(pass);
        attachment.setTexture(None);
        let Some(encoder) = encoder else {
            commands.endCommandBuffer();
            return Err(FrameError::EncoderUnavailable);
        };
        encoder.setArgumentTable_atStages(
            &self.argument_tables[slot],
            MTLRenderStages::Vertex | MTLRenderStages::Fragment,
        );
        if let Some(background) = frame.background {
            // The argument table already holds every slot buffer at its
            // SlotBuffer index, the shader's [[buffer(n)]].
            encoder.setRenderPipelineState(&background.state);
            #[allow(unsafe_code)]
            // SAFETY: FFI-DRAW. Three vertices of one triangle; bg_vertex
            // derives positions from vertex_id and reads no vertex buffer.
            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
        }
        if let Some(text) = frame.text {
            // The argument table holds the slot buffers and, bound above,
            // both atlases.
            encoder.setRenderPipelineState(&text.pipeline.state);
            #[allow(unsafe_code)]
            // SAFETY: FFI-DRAW. Four strip vertices per instance, positions
            // derived from vertex_id; text_vertex reads CellText only at
            // instance_id < instances, all of which the frame wrote into a
            // buffer fitted for them.
            unsafe {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                    text.instances,
                );
            }
        }
        encoder.endEncoding();
        commands.endCommandBuffer();
        if let Some(drawable) = drawable {
            self.queue.waitForDrawable(drawable);
        }
        let options = MTL4CommitOptions::new();
        let token = lease.submit();
        let failed = Arc::clone(&self.failed);
        let handler = RcBlock::new(
            move |feedback: NonNull<ProtocolObject<dyn MTL4CommitFeedback>>| {
                #[allow(unsafe_code)]
                // SAFETY: FFI-BLOCK. Metal passes the commit's feedback object,
                // valid for the duration of the handler.
                let error = unsafe { feedback.as_ref() }.error();
                if error.is_some() {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
                token.complete();
            },
        );
        #[allow(unsafe_code)]
        // SAFETY: FFI-BLOCK. Metal copies the block, so the local `handler`
        // may drop after this call. The closure owns only a CompletionToken
        // and an Arc<AtomicU64>, both Send + Sync and 'static, because Metal
        // runs feedback handlers on a queue of its own choosing.
        unsafe {
            options.addFeedbackHandler(RcBlock::as_ptr(&handler));
        }
        let mut committed = [NonNull::from(&**commands)];
        #[allow(unsafe_code)]
        // SAFETY: FFI-COMMIT. The pointer addresses a live one-element array
        // for the duration of the call and the count is 1. The command buffer
        // ended encoding above, and its allocator is not reset again until
        // this frame's feedback handler has freed the slot.
        unsafe {
            self.queue
                .commit_count_options(NonNull::from(&mut committed[0]), 1, &options);
        }
        if let Some(drawable) = drawable {
            self.queue.signalDrawable(drawable);
            drawable.present();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MetalDevice;
    use crate::cell_bg::{BackgroundUniforms, CellBgGrid, CursorShape, CursorUniform};
    use crate::frame::{FrameUniforms, SlotState, UNIFORMS_BYTES};
    use objc2::Message;
    use objc2_metal::{
        MTLBlitCommandEncoder, MTLCPUCacheMode, MTLPixelFormat, MTLSharedEvent, MTLStorageMode,
        MTLTextureDescriptor, MTLTextureUsage,
    };

    const LONG: Duration = Duration::from_secs(10);
    const SMALL: GridExtent = GridExtent { rows: 24, cols: 80 };
    /// The operator's 6K window.
    const LARGE: GridExtent = GridExtent {
        rows: 117,
        cols: 512,
    };

    fn device() -> Retained<ProtocolObject<dyn MTLDevice>> {
        MetalDevice::system_default()
            .unwrap_or_else(|reason| panic!("this test needs an admitted Metal device: {reason}"))
            .raw_device()
    }

    fn private_ledger() -> &'static GpuResourceLedger {
        Box::leak(Box::new(GpuResourceLedger::new()))
    }

    fn slots_u64() -> u64 {
        u64::try_from(FRAME_SLOTS).unwrap()
    }

    /// The buffers of one slot that resizing from `from` to `to` reallocates,
    /// derived from the sizing rules rather than hard-coded.
    fn grown_kinds(from: GridExtent, to: GridExtent) -> Vec<SlotBuffer> {
        let (from, to) = (
            SlotSizes::for_grid(from).unwrap(),
            SlotSizes::for_grid(to).unwrap(),
        );
        SlotBuffer::ALL
            .into_iter()
            .filter(|kind| {
                let initial = grown_capacity(0, from.bytes(*kind));
                grown_capacity(initial, to.bytes(*kind)) != initial
            })
            .collect()
    }

    fn offscreen_target(
        device: &ProtocolObject<dyn MTLDevice>,
    ) -> Retained<ProtocolObject<dyn MTLTexture>> {
        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. BGRA8Unorm is color-renderable and 64x64 is
        // within the Apple-family 2D texture limit.
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                64,
                64,
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        descriptor.setStorageMode(MTLStorageMode::Private);
        device
            .newTextureWithDescriptor(&descriptor)
            .expect("offscreen render target")
    }

    #[test]
    fn the_sizing_rules_grow_some_buffers_for_the_6k_window() {
        // Guards the derived expectations below against a vacuous pass.
        let grown = grown_kinds(SMALL, LARGE);
        assert!(grown.contains(&SlotBuffer::CellBg), "{grown:?}");
        assert!(grown.contains(&SlotBuffer::CellText), "{grown:?}");
        assert!(!grown.contains(&SlotBuffer::Uniforms), "{grown:?}");
    }

    #[test]
    fn slot_buffers_are_shared_write_combined_and_sized_for_the_grid() {
        let grid = GridExtent::new(80, 120);
        let sizes = SlotSizes::for_grid(grid).unwrap();
        let frames = FrameSlots::new(device(), grid, private_ledger()).unwrap();
        for slot in 0..FRAME_SLOTS {
            for kind in SlotBuffer::ALL {
                let buffer = frames.buffer(slot, kind);
                assert_eq!(
                    buffer.storageMode(),
                    MTLStorageMode::Shared,
                    "{slot} {kind:?}"
                );
                assert_eq!(
                    buffer.cpuCacheMode(),
                    MTLCPUCacheMode::WriteCombined,
                    "{slot} {kind:?}"
                );
                assert!(buffer.length() >= sizes.bytes(kind), "{slot} {kind:?}");
            }
        }
        assert_eq!(frames.allocations(), 12);
    }

    #[test]
    fn buffers_grow_geometrically_on_resize_and_never_in_steady_state() {
        let ledger = private_ledger();
        let mut frames = FrameSlots::new(device(), SMALL, ledger).unwrap();
        let live = |ledger: &GpuResourceLedger| ledger.snapshot().buffer_total.live_count;
        assert_eq!(live(ledger), 12);
        // A steady grid, then a smaller one: every slot visited, nothing
        // allocated.
        for grid in [SMALL, GridExtent::new(10, 40)] {
            for _ in 0..2 * FRAME_SLOTS {
                drop(frames.begin(grid, LONG).unwrap());
            }
        }
        assert_eq!(frames.allocations(), 12);
        // Each slot grows its too-small buffers once, when next leased.
        let grown = u64::try_from(grown_kinds(SMALL, LARGE).len()).unwrap();
        let sizes = SlotSizes::for_grid(LARGE).unwrap();
        for round in 0..2 {
            for slot in 0..FRAME_SLOTS {
                let lease = frames.begin(LARGE, LONG).unwrap();
                assert_eq!(lease.slot(), slot);
                for kind in SlotBuffer::ALL {
                    let length = frames.buffer(slot, kind).length();
                    assert!(length >= sizes.bytes(kind), "{kind:?}");
                    assert!(length.is_power_of_two(), "{kind:?} {length}");
                }
                assert_eq!(frames.generation(slot), 1, "round {round} slot {slot}");
            }
        }
        assert_eq!(frames.allocations(), 12 + slots_u64() * grown);
        // Replaced buffers left the ledger together with their buffers.
        assert_eq!(live(ledger), 12);
        assert_eq!(
            ledger.snapshot().buffer_total.created_total,
            12 + slots_u64() * grown
        );
    }

    #[test]
    fn residency_set_holds_every_slot_buffer_and_follows_growth() {
        let mut frames = FrameSlots::new(device(), SMALL, private_ledger()).unwrap();
        let set = frames.residency().expect("macOS 15+ has residency sets");
        assert_eq!(set.allocationCount(), 12);
        let slot0_before: Vec<_> = SlotBuffer::ALL
            .iter()
            .map(|kind| frames.buffer(0, *kind).retain())
            .collect();
        for slot in 0..FRAME_SLOTS {
            for kind in SlotBuffer::ALL {
                assert!(
                    set.containsAllocation(ProtocolObject::from_ref(frames.buffer(slot, kind)))
                );
            }
        }
        drop(frames.begin(LARGE, LONG).unwrap());
        let set = frames.residency().unwrap();
        assert_eq!(
            set.allocationCount(),
            12,
            "growth swaps allocations, it never adds"
        );
        for kind in SlotBuffer::ALL {
            assert!(set.containsAllocation(ProtocolObject::from_ref(frames.buffer(0, kind))));
        }
        let left = SlotBuffer::ALL
            .iter()
            .filter(|kind| {
                !set.containsAllocation(ProtocolObject::from_ref(&*slot0_before[kind.index()]))
            })
            .count();
        assert_eq!(
            left,
            grown_kinds(SMALL, LARGE).len(),
            "exactly slot 0's replaced buffers left the set"
        );
    }

    #[test]
    fn writes_are_bounds_checked() {
        let grid = GridExtent::new(2, 2);
        let mut frames = FrameSlots::new(device(), grid, private_ledger()).unwrap();
        let lease = frames.begin(grid, LONG).unwrap();
        let capacity = frames.buffer(lease.slot(), SlotBuffer::Uniforms).length();
        assert!(
            frames
                .write(&lease, SlotBuffer::Uniforms, capacity - 4, &[1; 4])
                .is_ok()
        );
        assert_eq!(
            frames.write(&lease, SlotBuffer::Uniforms, capacity - 3, &[1; 4]),
            Err(FrameError::SlotWriteOutOfBounds {
                buffer: "uniforms",
                offset: capacity - 3,
                len: 4,
                capacity,
            })
        );
        assert!(
            frames
                .write(&lease, SlotBuffer::Uniforms, usize::MAX, &[1])
                .is_err()
        );
    }

    /// The bead's native test. Each frame's GPU work first waits on a shared
    /// event the test controls, so a frame stays in flight until the test
    /// releases it, then copies the slot's uniform block into a readback
    /// buffer. No slot may be leased while all three frames are held, and
    /// every frame's readback must hold its own frame number: had the CPU
    /// rewritten a slot while its frame was in flight, the GPU would have
    /// copied a later frame's number.
    #[test]
    fn in_flight_slot_data_survives_artificially_slow_gpu_completion() {
        const FRAMES: u64 = 7;
        let device = device();
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let queue = device.newCommandQueue().unwrap();
        let gate = device.newSharedEvent().unwrap();
        let region = |frame: u64| UNIFORMS_BYTES * usize::try_from(frame).unwrap();
        let readback = device
            .newBufferWithLength_options(region(FRAMES), MTLResourceOptions::StorageModeShared)
            .unwrap();
        let (mut leased, mut released, mut refusals) = (0_u64, 0_u64, 0_u64);
        while leased < FRAMES {
            let lease = match frames.begin(SMALL, Duration::from_millis(50)) {
                Ok(lease) => lease,
                Err(FrameError::FrameSlotTimeout { in_flight, .. }) => {
                    if leased - released == slots_u64() {
                        // Every frame is held behind the gate, so the GPU
                        // cannot have finished one: the refusal is right.
                        assert_eq!(in_flight, FRAME_SLOTS);
                        refusals += 1;
                        released += 1;
                        gate.setSignaledValue(released);
                    }
                    // Otherwise a released frame's completion handler has not
                    // run yet; try again.
                    continue;
                }
                Err(other) => panic!("{other}"),
            };
            assert!(
                leased - released < slots_u64(),
                "slot {} was leased while all {FRAME_SLOTS} frames were held in flight",
                lease.slot()
            );
            assert_eq!(lease.frame(), leased);
            let uniforms = FrameUniforms {
                frame: lease.frame(),
                viewport: [64, 64],
                grid: SMALL,
                clear: [0.0, 0.0, 0.0, 1.0],
                ..FrameUniforms::default()
            };
            frames
                .write(&lease, SlotBuffer::Uniforms, 0, &uniforms.to_bytes())
                .unwrap();
            let commands = queue.commandBuffer().unwrap();
            commands.encodeWaitForEvent_value(ProtocolObject::from_ref(&*gate), lease.frame() + 1);
            let blit = commands.blitCommandEncoder().unwrap();
            #[allow(unsafe_code)]
            // SAFETY: FFI-EXTENT. Source: the slot's uniform block, at least
            // UNIFORMS_BYTES long. Destination: the frame's UNIFORMS_BYTES
            // region of `readback`, which holds FRAMES of them.
            unsafe {
                blit.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
                    frames.buffer(lease.slot(), SlotBuffer::Uniforms),
                    0,
                    &readback,
                    region(lease.frame()),
                    UNIFORMS_BYTES,
                );
            }
            blit.endEncoding();
            let token = lease.submit();
            let handler = RcBlock::new(move |_: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                token.complete();
            });
            #[allow(unsafe_code)]
            // SAFETY: FFI-BLOCK. As in Metal3Submission::encode_clear: Metal
            // copies the block, whose closure owns only a Send + Sync token.
            unsafe {
                commands.addCompletedHandler(RcBlock::as_ptr(&handler));
            }
            commands.commit();
            leased += 1;
        }
        gate.setSignaledValue(FRAMES);
        assert!(frames.ring().wait_idle(LONG), "every frame completed");
        assert_eq!(
            refusals,
            FRAMES - slots_u64(),
            "one refusal per frame past the third"
        );
        assert_eq!(frames.ring().peak_in_flight(), FRAME_SLOTS);
        let contents = readback.contents().cast::<u8>();
        #[allow(unsafe_code)]
        // SAFETY: BUFFER-CONTENTS. Every command buffer that wrote `readback`
        // has completed (the ring is idle), and it is region(FRAMES) long.
        let bytes =
            unsafe { std::slice::from_raw_parts(contents.as_ptr(), region(FRAMES)) }.to_vec();
        for frame in 0..FRAMES {
            let block = &bytes[region(frame)..region(frame + 1)];
            assert_eq!(
                FrameUniforms::frame_of(block),
                Some(frame),
                "the GPU saw another frame's data in frame {frame}'s slot"
            );
        }
    }

    /// Runs `count` cleared frames on an offscreen target through
    /// `submission`, after growing to `grid` on the first frame.
    fn run_offscreen_frames(
        submission: &Submission,
        frames: &mut FrameSlots,
        grid: GridExtent,
        count: u64,
    ) {
        let target = offscreen_target(&frames.device.clone());
        if let Some(set) = frames.residency() {
            // Metal 4 makes nothing resident implicitly: the render target
            // stands in for the layer's drawables.
            set.addAllocation(ProtocolObject::from_ref(&*target));
            set.commit();
        }
        let color = ClearColor::from_srgba(0.1, 0.2, 0.3, 1.0);
        for _ in 0..count {
            let lease = frames.begin(grid, LONG).unwrap();
            let uniforms = FrameUniforms {
                frame: lease.frame(),
                viewport: [64, 64],
                grid,
                clear: [0.1, 0.2, 0.3, 1.0],
                ..FrameUniforms::default()
            };
            frames
                .write(&lease, SlotBuffer::Uniforms, 0, &uniforms.to_bytes())
                .unwrap();
            submission
                .encode_frame(frames, lease, &target, None, color, None, None)
                .unwrap();
        }
        assert!(frames.ring().wait_idle(LONG), "every frame completed");
        if let Some(set) = frames.residency() {
            set.removeAllocation(ProtocolObject::from_ref(&*target));
            set.commit();
        }
    }

    #[test]
    fn metal3_frames_complete_and_allocate_nothing_in_steady_state() {
        let device = device();
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let submission = Submission::metal3(device.newCommandQueue().unwrap(), &frames);
        assert_eq!(submission.path(), SubmissionPath::Metal3);
        run_offscreen_frames(&submission, &mut frames, SMALL, 30);
        assert_eq!(frames.ring().frames_completed(), 30);
        assert_eq!(submission.failed_frames(), 0);
        assert_eq!(frames.allocations(), 12);
        assert!(frames.ring().peak_in_flight() <= FRAME_SLOTS);
        for slot in 0..FRAME_SLOTS {
            assert_eq!(frames.ring().state(slot), SlotState::Free);
            let uniforms = frames.read(slot, SlotBuffer::Uniforms, UNIFORMS_BYTES);
            let frame = FrameUniforms::frame_of(&uniforms).unwrap();
            assert_eq!(
                frame % slots_u64(),
                u64::try_from(slot).unwrap(),
                "slot {slot} last held frame {frame}"
            );
        }
    }

    #[test]
    fn metal4_frames_reuse_allocators_and_argument_tables() {
        let device = device();
        if !supports_metal4(&device) {
            eprintln!("skipped: this device or OS has no Metal 4 command queues");
            return;
        }
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let submission = Submission::metal4(&device, &frames).expect("Metal 4 submission");
        assert_eq!(submission.path(), SubmissionPath::Metal4);
        run_offscreen_frames(&submission, &mut frames, SMALL, 30);
        assert_eq!(frames.ring().frames_completed(), 30);
        assert_eq!(submission.failed_frames(), 0);
        assert_eq!(frames.allocations(), 12);
        let Submission::Metal4(metal4) = &submission else {
            unreachable!("Submission::metal4 returns the Metal 4 path")
        };
        for (slot, bound) in metal4.bound.iter().enumerate() {
            assert_eq!(bound.get(), Some(0), "slot {slot} bound once");
        }
        // The ring is idle after 30 frames, so the next lease is slot 0: a
        // resize rebinds only the slot that grew, on its own next frame.
        run_offscreen_frames(&submission, &mut frames, LARGE, 1);
        assert_eq!(metal4.bound[0].get(), Some(1));
        assert_eq!(metal4.bound[1].get(), Some(0));
        assert_eq!(metal4.bound[2].get(), Some(0));
        assert_eq!(submission.failed_frames(), 0);
    }

    #[test]
    fn metal4_selection_matches_the_device() {
        let device = device();
        let frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let metal4 = Submission::metal4(&device, &frames);
        assert_eq!(
            metal4.is_ok(),
            supports_metal4(&device),
            "{:?}",
            metal4.err()
        );
    }

    // ---- Background pass (ft-yccm0.4.2.2) ----

    const BG_WIDTH: usize = 96;
    const BG_HEIGHT: usize = 103;

    /// Every background feature at once: colors and default cells, a ring
    /// offset, a wide character, a selection, search matches.
    fn background_scene() -> (CellBgGrid, BackgroundUniforms) {
        use crate::cell_bg::CellBg;
        let mut cells = CellBgGrid::new(GridExtent::new(6, 10));
        for row in 0..6_u8 {
            for col in 0..10_u8 {
                if (row + col) % 4 != 0 {
                    let bg = CellBg::rgb(row * 40, col * 25, 128);
                    cells.set(u32::from(row), u32::from(col), bg);
                }
            }
        }
        cells.scroll_up(2);
        cells.fill_row(4, CellBg::rgb(10, 200, 30));
        cells.set_wide(5, 2, CellBg::rgb(250, 250, 0));
        for col in 1..5 {
            let bg = cells.get(2, col).unwrap();
            cells.set(2, col, bg.selected());
        }
        for col in 2..4 {
            let bg = cells.get(3, col).unwrap();
            cells.set(3, col, bg.search_match());
        }
        let bg = cells.get(3, 6).unwrap();
        cells.set(3, 6, bg.search_match().current_match());
        let background = BackgroundUniforms {
            cell_size: [8.0, 16.0],
            grid_origin: [4.0, 2.0],
            row_offset: 0,
            cursor: CursorUniform::default(),
            selection_tint: [0.0, 0.0, 0.3, 0.3],
            search_tint: [0.4, 0.4, 0.0, 0.4],
            current_match_tint: [0.5, 0.25, 0.0, 0.5],
        };
        (cells, background)
    }

    /// Renders the scene with `cursor` through the production offscreen path
    /// (`render_cells_offscreen`, as the render snapshot uses it) and
    /// compares every pixel with the CPU reference; returns the pixels
    /// compared.
    fn render_and_compare(
        submission: &Submission,
        frames: &mut FrameSlots,
        pipeline: &BackgroundPipeline,
        cursor: CursorUniform,
    ) -> usize {
        let (cells, background) = background_scene();
        let background = BackgroundUniforms {
            cursor,
            ..background
        };
        let clear = ClearColor::from_srgba(0.1, 0.2, 0.3, 1.0);
        let (width, height) = (
            u32::try_from(BG_WIDTH).unwrap(),
            u32::try_from(BG_HEIGHT).unwrap(),
        );
        let readback = frames.device.newCommandQueue().unwrap();
        let pixels = frames
            .render_cells_offscreen(
                submission,
                pipeline,
                &readback,
                &OffscreenCells {
                    width,
                    height,
                    cells: &cells,
                    clear,
                    background,
                    text: None,
                },
            )
            .unwrap_or_else(|error| panic!("{:?} cursor: {error}", cursor.shape));
        assert_eq!(pixels.len(), BG_WIDTH * BG_HEIGHT * 4);
        assert!(frames.ring().wait_idle(LONG));
        assert_eq!(submission.failed_frames(), 0);
        let uniforms = FrameUniforms {
            viewport: [width, height],
            grid: cells.extent(),
            clear: clear.to_f32(),
            background: BackgroundUniforms {
                row_offset: cells.row_offset(),
                ..background
            },
            ..FrameUniforms::default()
        };
        let cleared = clear.to_bgra8();
        let mut compared = 0;
        for y in 0..BG_HEIGHT {
            for x in 0..BG_WIDTH {
                let center = |v: usize| f32::from(u16::try_from(v).unwrap()) + 0.5;
                let expected =
                    crate::cell_bg::shade_background(&uniforms, &cells, center(x), center(y))
                        .map_or(cleared, crate::cell_bg::to_bgra8);
                let at = (y * BG_WIDTH + x) * 4;
                let got = &pixels[at..at + 4];
                let off = got
                    .iter()
                    .zip(expected)
                    .map(|(got, want)| got.abs_diff(want))
                    .max()
                    .unwrap();
                assert!(
                    off <= 1,
                    "{:?} cursor, pixel ({x}, {y}): GPU {got:?}, reference {expected:?}",
                    cursor.shape
                );
                compared += 1;
            }
        }
        compared
    }

    #[test]
    fn offscreen_renders_reject_bad_extents() {
        let device = device();
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let submission = Submission::metal3(device.newCommandQueue().unwrap(), &frames);
        let pipeline = BackgroundPipeline::new(&device).expect("background pipeline");
        let readback = device.newCommandQueue().unwrap();
        let (cells, background) = background_scene();
        for (width, height) in [(0, 10), (10, 0), (crate::MAX_TEXTURE_EXTENT + 1, 10)] {
            let request = OffscreenCells {
                width,
                height,
                cells: &cells,
                clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
                background,
                text: None,
            };
            assert_eq!(
                frames.render_cells_offscreen(&submission, &pipeline, &readback, &request),
                Err(FrameError::InvalidExtent { width, height })
            );
        }
        assert_eq!(
            frames.residency().map(MTLResidencySet::allocationCount),
            Some(12),
            "a refused render leaves the residency set as it was"
        );
    }

    fn cursors() -> Vec<CursorUniform> {
        let white = [1.0, 1.0, 1.0, 1.0];
        let at = |shape, col, width_cells| CursorUniform {
            shape,
            col,
            row: 1,
            width_cells,
            thickness: 2.0,
            color: white,
        };
        vec![
            CursorUniform::default(),
            at(CursorShape::Block, 3, 1),
            at(CursorShape::HollowBlock, 3, 1),
            at(CursorShape::Underline, 7, 1),
            at(CursorShape::Bar, 0, 1),
            // Over the wide character at row 5, column 2.
            CursorUniform {
                row: 5,
                ..at(CursorShape::Block, 2, 2)
            },
        ]
    }

    #[test]
    fn metal3_background_pass_matches_the_cpu_reference() {
        let device = device();
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let submission = Submission::metal3(device.newCommandQueue().unwrap(), &frames);
        let pipeline = BackgroundPipeline::new(&device).expect("background pipeline");
        for cursor in cursors() {
            assert_eq!(
                render_and_compare(&submission, &mut frames, &pipeline, cursor),
                BG_WIDTH * BG_HEIGHT
            );
        }
    }

    #[test]
    fn metal4_background_pass_matches_the_cpu_reference() {
        let device = device();
        if !supports_metal4(&device) {
            eprintln!("skipped: this device or OS has no Metal 4 command queues");
            return;
        }
        let mut frames = FrameSlots::new(device.clone(), SMALL, private_ledger()).unwrap();
        let submission = Submission::metal4(&device, &frames).expect("Metal 4 submission");
        let pipeline = BackgroundPipeline::new(&device).expect("background pipeline");
        for cursor in cursors() {
            assert_eq!(
                render_and_compare(&submission, &mut frames, &pipeline, cursor),
                BG_WIDTH * BG_HEIGHT
            );
        }
    }

    // ---- Text pass (ft-yccm0.4.2.3) ----

    use crate::atlas::{AtlasConfig, AtlasSlot, FrameFence, texture_bytes};
    use crate::cell_bg::to_bgra8;
    use crate::cell_text::{CellText, UnderlineStyle, shade_text};

    /// Everything a text-pass render needs, on one submission path.
    struct TextFixture {
        frames: FrameSlots,
        submission: Submission,
        background: BackgroundPipeline,
        text: TextPipeline,
        atlases: GlyphAtlases,
        /// Every glyph placed in the atlases, with its pixels.
        glyphs: Vec<(AtlasSlot, Vec<u8>)>,
    }

    fn small_atlas(kind: AtlasKind) -> AtlasConfig {
        AtlasConfig {
            width: 256,
            initial_height: 128,
            page_height: 64,
            padding: 1,
            max_bytes: texture_bytes(kind, 256, 128),
        }
    }

    impl TextFixture {
        /// `metal4` selects the Metal 4 path; `None` when this device has
        /// none.
        fn new(metal4: bool) -> Option<Self> {
            let metal = MetalDevice::system_default()
                .unwrap_or_else(|reason| panic!("this test needs a Metal device: {reason}"));
            let device = metal.raw_device();
            if metal4 && !supports_metal4(&device) {
                eprintln!("skipped: this device or OS has no Metal 4 command queues");
                return None;
            }
            // Sized for the background scene, so a crowded frame outgrows
            // the reserved CellText buffer.
            let frames =
                FrameSlots::new(device.clone(), GridExtent::new(6, 10), private_ledger()).unwrap();
            let submission = if metal4 {
                Submission::metal4(&device, &frames).expect("Metal 4 submission")
            } else {
                Submission::metal3(device.newCommandQueue().unwrap(), &frames)
            };
            let mut atlases = GlyphAtlases::new(
                &metal,
                small_atlas(AtlasKind::Grayscale),
                small_atlas(AtlasKind::Color),
                false,
                private_ledger(),
            )
            .unwrap();
            if let Some(set) = atlases.residency() {
                submission.add_residency_set(set);
            }
            let fence = FrameFence {
                current: frames.ring().next_frame(),
                retired_before: frames.ring().retired_before(),
            };
            let mut glyphs = Vec::new();
            for (kind, width, height) in [
                (AtlasKind::Grayscale, 6, 12),
                (AtlasKind::Grayscale, 8, 16),
                (AtlasKind::Color, 16, 16),
            ] {
                let pixels = glyph_pixels(kind, width, height);
                let slot = atlases
                    .insert(&metal, kind, width, height, &pixels, fence)
                    .unwrap();
                glyphs.push((slot, pixels));
            }
            Some(Self {
                background: BackgroundPipeline::new(&device).expect("background pipeline"),
                text: TextPipeline::new(&device).expect("text pipeline"),
                frames,
                submission,
                atlases,
                glyphs,
            })
        }

        fn glyph(&self, index: usize) -> AtlasSlot {
            self.glyphs[index].0
        }

        /// The stored bytes of an atlas texel, as `shade_text` takes them.
        /// Panics outside every glyph: the shader never samples there.
        fn texel(&self, kind: AtlasKind, x: u32, y: u32) -> [u8; 4] {
            let (slot, pixels) = self
                .glyphs
                .iter()
                .find(|(slot, _)| {
                    slot.kind == kind
                        && (slot.x..slot.x + slot.width).contains(&x)
                        && (slot.y..slot.y + slot.height).contains(&y)
                })
                .unwrap_or_else(|| panic!("{kind:?} texel ({x}, {y}) is in no glyph"));
            let index = ((y - slot.y) * slot.width + (x - slot.x)) as usize;
            match kind {
                AtlasKind::Grayscale => [pixels[index], 0, 0, 0],
                AtlasKind::Color => pixels[index * 4..index * 4 + 4].try_into().unwrap(),
            }
        }

        fn render(&mut self, cells: &CellBgGrid, request: TextRequest<'_>) -> Vec<u8> {
            let readback = self.frames.device.newCommandQueue().unwrap();
            let pixels = self
                .frames
                .render_cells_offscreen(
                    &self.submission,
                    &self.background,
                    &readback,
                    &OffscreenCells {
                        width: request.width,
                        height: request.height,
                        cells,
                        clear: request.clear,
                        background: request.background,
                        text: Some(OffscreenText {
                            grid: request.text,
                            uniforms: request.uniforms,
                            pipeline: &self.text,
                            atlases: &self.atlases,
                        }),
                    },
                )
                .unwrap();
            assert!(self.frames.ring().wait_idle(LONG));
            assert_eq!(self.submission.failed_frames(), 0);
            pixels
        }
    }

    #[derive(Clone, Copy)]
    struct TextRequest<'a> {
        width: u32,
        height: u32,
        clear: ClearColor,
        background: BackgroundUniforms,
        text: &'a CellTextGrid,
        uniforms: TextUniforms,
    }

    /// Distinct pixels for every texel: grayscale coverage that sweeps 0 to
    /// 255, color texels premultiplied with varying alpha, `[B, G, R, A]`.
    fn glyph_pixels(kind: AtlasKind, width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let wave = u8::try_from((x * 41 + y * 23 + width) % 256).unwrap();
                match kind {
                    AtlasKind::Grayscale => pixels.push(if width == 8 { 255 } else { wave }),
                    AtlasKind::Color => {
                        let alpha = u16::from(wave.max(1));
                        let part =
                            |n: u32| u8::try_from(alpha * u16::try_from(n).unwrap() / 15).unwrap();
                        pixels.extend([
                            part(15 - x.min(15)),
                            part(y.min(15)),
                            part(x.min(15)),
                            wave.max(1),
                        ]);
                    }
                }
            }
        }
        pixels
    }

    /// Every text feature at once over the background scene: tinted and
    /// translucent grayscale glyphs, wide color emoji, every underline style
    /// in its own color, overline, strikethrough, glyphs overhanging their
    /// cell and the grid, stacked instances, a decoration without a glyph,
    /// and the ring offset the background scene scrolled to.
    fn text_scene(fixture: &TextFixture, cells: &CellBgGrid) -> CellTextGrid {
        let mut text = CellTextGrid::new(cells.extent());
        text.scroll_up(cells.row_offset());
        assert_eq!(text.row_offset(), cells.row_offset());
        let (gray, block, emoji) = (fixture.glyph(0), fixture.glyph(1), fixture.glyph(2));
        let colors = [
            [255, 255, 255, 255],
            [250, 40, 10, 255],
            [20, 220, 90, 128],
            [30, 60, 240, 200],
            [0, 0, 0, 255],
        ];
        for (col, fg) in (0_u16..).zip(colors) {
            text.push(0, CellText::new(col, fg).with_glyph(&gray, [1, 2]));
        }
        text.push(
            1,
            CellText::new(1, [255; 4]).with_glyph(&emoji, [0, 0]).wide(),
        );
        text.push(
            1,
            CellText::new(4, [255, 255, 255, 100])
                .with_glyph(&emoji, [0, 0])
                .wide(),
        );
        let styles = [
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ];
        for (col, style) in (0_u16..).zip(styles) {
            let fg = [200, 200, 200, 255];
            let underline = [u8::try_from(col).unwrap() * 50, 255, 60];
            text.push(2, CellText::new(col, fg).with_underline(style, underline));
        }
        text.push(2, CellText::new(5, [255, 0, 255, 255]).with_strikethrough());
        text.push(2, CellText::new(6, [0, 255, 255, 255]).with_overline());
        text.push(
            2,
            CellText::new(7, [240, 240, 0, 255])
                .with_glyph(&block, [0, 0])
                .with_underline(UnderlineStyle::Single, [0, 0, 255])
                .with_overline()
                .with_strikethrough(),
        );
        // Overhangs: past the grid's right edge, into the left padding,
        // into the row above.
        text.push(3, CellText::new(9, [255; 4]).with_glyph(&gray, [5, -3]));
        text.push(3, CellText::new(0, [255; 4]).with_glyph(&gray, [-3, 0]));
        // Two glyphs stacked in one cell, the second translucent.
        text.push(
            4,
            CellText::new(3, [255, 128, 0, 255]).with_glyph(&gray, [0, 0]),
        );
        text.push(
            4,
            CellText::new(3, [0, 128, 255, 120]).with_glyph(&gray, [2, 4]),
        );
        text.push(
            5,
            CellText::new(8, [255; 4]).with_glyph(&emoji, [0, 0]).wide(),
        );
        text
    }

    /// The color the GPU blends onto: the background pass's 8-bit output.
    fn from_bgra8(bytes: [u8; 4]) -> [f32; 4] {
        let unit = |byte: u8| f32::from(byte) / 255.0;
        [
            unit(bytes[2]),
            unit(bytes[1]),
            unit(bytes[0]),
            unit(bytes[3]),
        ]
    }

    /// Renders `text` over the background scene and compares every pixel
    /// with the CPU reference (`shade_background`, then `shade_text`).
    /// Pixels may differ by more than one step only inside `loose` (the
    /// curly underline's cell, where `sin` rounding can move a boundary
    /// pixel), at most four of them. Returns the pixels compared.
    fn render_text_and_compare(
        fixture: &mut TextFixture,
        text: &CellTextGrid,
        uniforms: TextUniforms,
        loose: Option<(u32, u32)>,
    ) -> usize {
        let (cells, background) = background_scene();
        render_grids_and_compare(fixture, &cells, background, text, uniforms, loose)
    }

    /// [`render_text_and_compare`] for any cell grid: renders `cells` and
    /// `text` through the production offscreen path and compares every
    /// pixel with the CPU reference; returns the pixels compared.
    #[allow(clippy::cast_precision_loss)]
    fn render_grids_and_compare(
        fixture: &mut TextFixture,
        cells: &CellBgGrid,
        background: BackgroundUniforms,
        text: &CellTextGrid,
        uniforms: TextUniforms,
        loose: Option<(u32, u32)>,
    ) -> usize {
        let clear = ClearColor::from_srgba(0.1, 0.2, 0.3, 1.0);
        let request = TextRequest {
            width: u32::try_from(BG_WIDTH).unwrap(),
            height: u32::try_from(BG_HEIGHT).unwrap(),
            clear,
            background,
            text,
            uniforms,
        };
        let pixels = fixture.render(cells, request);
        assert_eq!(pixels.len(), BG_WIDTH * BG_HEIGHT * 4);
        let frame = FrameUniforms {
            viewport: [request.width, request.height],
            grid: cells.extent(),
            clear: clear.to_f32(),
            background: BackgroundUniforms {
                row_offset: cells.row_offset(),
                ..background
            },
            text: uniforms,
            ..FrameUniforms::default()
        };
        let (mut compared, mut loose_misses) = (0, 0);
        for y in 0..BG_HEIGHT {
            for x in 0..BG_WIDTH {
                let center = |v: usize| f32::from(u16::try_from(v).unwrap()) + 0.5;
                let (cx, cy) = (center(x), center(y));
                let under = crate::cell_bg::shade_background(&frame, cells, cx, cy)
                    .map_or(clear.to_bgra8(), to_bgra8);
                let expected = to_bgra8(shade_text(
                    &frame,
                    text,
                    |kind, ax, ay| fixture.texel(kind, ax, ay),
                    from_bgra8(under),
                    cx,
                    cy,
                ));
                let at = (y * BG_WIDTH + x) * 4;
                let got = &pixels[at..at + 4];
                let off = got
                    .iter()
                    .zip(expected)
                    .map(|(got, want)| got.abs_diff(want))
                    .max()
                    .unwrap();
                compared += 1;
                if off <= 1 {
                    continue;
                }
                let in_loose = loose.is_some_and(|(row, col)| {
                    let left = background.grid_origin[0] + col as f32 * background.cell_size[0];
                    let top = background.grid_origin[1] + row as f32 * background.cell_size[1];
                    (left..left + background.cell_size[0]).contains(&cx)
                        && (top..top + background.cell_size[1]).contains(&cy)
                });
                assert!(
                    in_loose,
                    "pixel ({x}, {y}): GPU {got:?}, reference {expected:?}"
                );
                loose_misses += 1;
            }
        }
        assert!(
            loose_misses <= 4,
            "{loose_misses} curly boundary pixels differ"
        );
        compared
    }

    fn check_text_pass(metal4: bool) {
        let Some(mut fixture) = TextFixture::new(metal4) else {
            return;
        };
        let (cells, _) = background_scene();
        let text = text_scene(&fixture, &cells);
        // The curly underline is at logical row 2, column 2.
        for thickness in [1.0, 2.0] {
            let uniforms = TextUniforms {
                underline_position: 13.0,
                line_thickness: thickness,
                strikethrough_position: 8.0,
            };
            assert_eq!(
                render_text_and_compare(&mut fixture, &text, uniforms, Some((2, 2))),
                BG_WIDTH * BG_HEIGHT
            );
        }
    }

    #[test]
    fn metal3_text_pass_matches_the_cpu_reference() {
        check_text_pass(false);
    }

    #[test]
    fn metal4_text_pass_matches_the_cpu_reference() {
        check_text_pass(true);
    }

    /// Every slot ends up holding the grids' region layout (ft-yccm0.4.2.4)
    /// of instances and row table, and steady-state text frames allocate no
    /// buffer, touch no atlas and upload nothing.
    #[test]
    fn text_frames_upload_the_instances_and_row_table_and_allocate_nothing() {
        let mut fixture = TextFixture::new(false).unwrap();
        let (cells, background) = background_scene();
        let text = text_scene(&fixture, &cells);
        let uniforms = TextUniforms {
            underline_position: 13.0,
            line_thickness: 1.0,
            strikethrough_position: 8.0,
        };
        let request = TextRequest {
            width: 64,
            height: 64,
            clear: ClearColor::from_srgba(0.0, 0.0, 0.0, 1.0),
            background,
            text: &text,
            uniforms,
        };
        fixture.render(&cells, request);
        let allocations = fixture.frames.allocations();
        let generations = AtlasKind::ALL.map(|kind| fixture.atlases.allocator(kind).generation());
        for _ in 0..6 {
            fixture.render(&cells, request);
        }
        assert_eq!(fixture.frames.allocations(), allocations, "steady state");
        assert_eq!(
            AtlasKind::ALL.map(|kind| fixture.atlases.allocator(kind).generation()),
            generations
        );
        assert_eq!(fixture.atlases.retired(), 0);
        let last = usize::try_from((fixture.frames.ring().next_frame() - 1) % slots_u64()).unwrap();
        let capacity = crate::uploads::needed_capacity(&text);
        let (bg, instances, table) = crate::uploads::full_layout(&cells, &text, capacity);
        assert_eq!(fixture.frames.read(last, SlotBuffer::CellBg, bg.len()), bg);
        assert_eq!(
            fixture
                .frames
                .read(last, SlotBuffer::CellText, instances.len()),
            instances
        );
        assert_eq!(
            fixture.frames.read(last, SlotBuffer::RowTable, table.len()),
            table
        );
        assert_eq!(
            fixture.frames.upload_bytes().0,
            0,
            "steady frames upload nothing"
        );
    }

    /// Dirty-row uploads (ft-yccm0.4.2.4) on the GPU: frame after frame of
    /// cell writes, glyph pushes, wide color glyphs and scrolls, each frame
    /// uploading only what its slot had not seen, read back exactly as the
    /// CPU reference draws the current grids. Once the slots converge a
    /// one-row change costs one row in each of the next three frames, then
    /// nothing.
    #[test]
    fn dirty_row_frames_match_the_reference_and_upload_only_changed_rows() {
        use crate::cell_bg::CellBg;
        let mut fixture = TextFixture::new(false).unwrap();
        let (mut cells, background) = background_scene();
        let mut text = text_scene(&fixture, &cells);
        let uniforms = TextUniforms {
            underline_position: 13.0,
            line_thickness: 1.0,
            strikethrough_position: 8.0,
        };
        let (gray, color) = (fixture.glyph(0), fixture.glyph(2));
        let cols = cells.extent().cols;
        for frame in 0..18_u32 {
            let row = frame % 6;
            match frame % 3 {
                0 => {
                    text.clear_row(row);
                    let col = u16::try_from(frame % cols).unwrap();
                    text.push(
                        row,
                        CellText::new(col, [255, 128, 0, 255]).with_glyph(&gray, [1, 2]),
                    );
                }
                1 => {
                    let blue = u8::try_from(frame * 7 % 256).unwrap();
                    cells.set(row, frame % cols, CellBg::rgb(10, 200, blue));
                    text.push(
                        row,
                        CellText::new(3, [0, 0, 0, 255])
                            .with_glyph(&color, [0, 0])
                            .wide(),
                    );
                }
                _ => {
                    cells.scroll_up(1);
                    text.scroll_up(1);
                }
            }
            let compared =
                render_grids_and_compare(&mut fixture, &cells, background, &text, uniforms, None);
            assert_eq!(compared, BG_WIDTH * BG_HEIGHT, "frame {frame}");
        }
        for _ in 0..3 {
            render_grids_and_compare(&mut fixture, &cells, background, &text, uniforms, None);
        }
        assert_eq!(
            fixture.frames.upload_bytes().0,
            0,
            "converged slots upload nothing"
        );
        cells.set(1, 1, CellBg::rgb(1, 2, 3));
        for slot in 0..3 {
            render_grids_and_compare(&mut fixture, &cells, background, &text, uniforms, None);
            assert_eq!(
                fixture.frames.upload_bytes().0,
                u64::from(cols) * 4,
                "slot {slot} uploads the one changed background row"
            );
        }
        render_grids_and_compare(&mut fixture, &cells, background, &text, uniforms, None);
        assert_eq!(fixture.frames.upload_bytes().0, 0);
    }

    /// A frame with more instances than its slot reserves (stacked
    /// combining marks) grows only that slot's CellText buffer, and still
    /// draws every instance.
    #[test]
    fn crowded_text_frames_grow_cell_text_and_draw_every_instance() {
        let mut fixture = TextFixture::new(false).unwrap();
        let (cells, _) = background_scene();
        let mut text = CellTextGrid::new(cells.extent());
        text.scroll_up(cells.row_offset());
        let gray = fixture.glyph(0);
        for row in 0..6 {
            for col in 0..10_u16 {
                for layer in 0..8_i16 {
                    let fg = [255, u8::try_from(layer).unwrap() * 30, 0, 40];
                    text.push(
                        row,
                        CellText::new(col, fg).with_glyph(&gray, [layer % 3, layer]),
                    );
                }
            }
        }
        let reserved = SlotSizes::for_grid(cells.extent()).unwrap();
        let capacity = fixture.frames.buffer(0, SlotBuffer::CellText).length();
        assert!(
            text.len() * crate::frame::CELL_TEXT_INSTANCE_BYTES
                > capacity.max(reserved.bytes(SlotBuffer::CellText)),
            "the scene must overflow the reserved CellText buffer"
        );
        let before = fixture.frames.allocations();
        let uniforms = TextUniforms::default();
        assert_eq!(
            render_text_and_compare(&mut fixture, &text, uniforms, None),
            BG_WIDTH * BG_HEIGHT
        );
        assert_eq!(
            fixture.frames.allocations(),
            before + 1,
            "one CellText buffer"
        );
    }
}
