//! Frame slots on Metal (ft-yccm0.4.2.1).
//!
//! [`FrameSlots`] owns three slots of per-frame buffers in shared,
//! write-combined memory, paced by [`SlotRing`] and kept resident by one
//! `MTLResidencySet`. [`Submission`] encodes and commits a frame through
//! Metal 4 on macOS 26 (one command allocator, command buffer and argument
//! table per slot, created once and reused) or through Metal 3 (a command
//! buffer per frame, completion handlers) everywhere else. Both paths share
//! the buffer model and the pacing.

use crate::frame::{
    FRAME_SLOTS, GridExtent, SlotBuffer, SlotLease, SlotRing, SlotSizes, grown_capacity,
};
use crate::{ClearColor, FrameError, SubmissionPath};
use block2::RcBlock;
use frankenterm_alloc::resource_ledger::{GpuBufferPurpose, GpuResourceGuard, GpuResourceLedger};
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::sel;
use objc2_foundation::{NSString, ns_string};
use objc2_metal::{
    MTL4ArgumentTable, MTL4ArgumentTableDescriptor, MTL4CommandAllocator, MTL4CommandBuffer,
    MTL4CommandEncoder, MTL4CommandQueue, MTL4CommitFeedback, MTL4CommitOptions,
    MTL4RenderCommandEncoder, MTL4RenderPassDescriptor, MTLBuffer, MTLClearColor, MTLCommandBuffer,
    MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLDrawable,
    MTLGPUFamily, MTLLoadAction, MTLRenderPassColorAttachmentDescriptor,
    MTLRenderPassColorAttachmentDescriptorArray, MTLRenderPassDescriptor, MTLRenderStages,
    MTLResidencySet, MTLResidencySetDescriptor, MTLResource, MTLResourceOptions, MTLStoreAction,
    MTLTexture,
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
}

/// The renderer's frame slots; see the module docs.
pub(crate) struct FrameSlots {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    ring: Arc<SlotRing>,
    slots: Vec<Slot>,
    residency: Option<Retained<ProtocolObject<dyn MTLResidencySet>>>,
    ledger: &'static GpuResourceLedger,
    allocations: u64,
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
        };
        for index in 0..FRAME_SLOTS {
            let mut buffers = Vec::with_capacity(SlotBuffer::ALL.len());
            for kind in SlotBuffer::ALL {
                buffers.push(this.allocate(kind, grown_capacity(0, sizes.bytes(kind)), index)?);
            }
            this.slots.push(Slot {
                buffers,
                generation: 0,
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

    /// Leases the next slot (waiting up to `timeout` for the GPU to finish
    /// the frame that last used it) and fits its buffers to `grid`.
    pub(crate) fn begin(
        &mut self,
        grid: GridExtent,
        timeout: Duration,
    ) -> Result<SlotLease, FrameError> {
        let sizes = sizes_for(grid)?;
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

    /// Frames whose command buffer finished in error.
    pub(crate) fn failed_frames(&self) -> u64 {
        match self {
            Self::Metal3(metal3) => metal3.failed.load(Ordering::Relaxed),
            Self::Metal4(metal4) => metal4.failed.load(Ordering::Relaxed),
        }
    }

    /// Encodes one frame into the leased slot: a render pass that clears
    /// `target` to `color`, presenting `drawable` when given, and commits it.
    /// The slot stays in flight until the GPU finishes the frame.
    pub(crate) fn encode_clear(
        &self,
        frames: &FrameSlots,
        lease: SlotLease,
        target: &ProtocolObject<dyn MTLTexture>,
        drawable: Option<&ProtocolObject<dyn MTLDrawable>>,
        color: ClearColor,
    ) -> Result<(), FrameError> {
        assert!(
            lease.is_from(frames.ring()),
            "a slot lease from another renderer's ring"
        );
        match self {
            Self::Metal3(metal3) => metal3.encode_clear(lease, target, drawable, color),
            Self::Metal4(metal4) => metal4.encode_clear(frames, lease, target, drawable, color),
        }
    }
}

impl Metal3Submission {
    fn encode_clear(
        &self,
        lease: SlotLease,
        target: &ProtocolObject<dyn MTLTexture>,
        drawable: Option<&ProtocolObject<dyn MTLDrawable>>,
        color: ClearColor,
    ) -> Result<(), FrameError> {
        let commands = self
            .queue
            .commandBuffer()
            .ok_or(FrameError::CommandBufferUnavailable)?;
        let pass = &self.passes[lease.slot()];
        let attachment = color_attachment(&pass.colorAttachments());
        configure_clear(&attachment, target, color);
        let encoder = commands.renderCommandEncoderWithDescriptor(pass);
        // The descriptor outlives the frame; it must not keep the drawable's
        // texture alive.
        attachment.setTexture(None);
        let encoder = encoder.ok_or(FrameError::EncoderUnavailable)?;
        encoder.endEncoding();
        if let Some(drawable) = drawable {
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

    fn encode_clear(
        &self,
        frames: &FrameSlots,
        lease: SlotLease,
        target: &ProtocolObject<dyn MTLTexture>,
        drawable: Option<&ProtocolObject<dyn MTLDrawable>>,
        color: ClearColor,
    ) -> Result<(), FrameError> {
        let slot = lease.slot();
        self.bind_slot(frames, slot);
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
            };
            frames
                .write(&lease, SlotBuffer::Uniforms, 0, &uniforms.to_bytes())
                .unwrap();
            submission
                .encode_clear(frames, lease, &target, None, color)
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
}
