//! Glyph atlas textures on Metal (ft-yccm0.4.3.2): the GPU half of
//! [`crate::atlas`].
//!
//! [`GlyphAtlases`] owns the two atlas textures, `R8Unorm` for grayscale
//! coverage and `BGRA8Unorm` (or its sRGB variant) for color glyphs, both in
//! shared storage, created at their full initial size up front and kept in
//! one residency set. A new glyph's pixels are written straight into its
//! region with `replaceRegion`; the allocator only hands out regions that no
//! unfinished frame reads, so there is no staging copy and no full-atlas
//! upload. When an atlas grows, a blit copies the old texture's rows into
//! the new, larger one, and the old texture is retired until every frame
//! that may have sampled it has finished. Both textures, and every retired
//! one, are recorded in the GPU resource ledger.
//!
//! Nothing samples the atlases yet: the glyph draw (ft-yccm0.4.2.3) binds
//! [`GlyphAtlases::texture`] and adds [`GlyphAtlases::residency`] to its queue.

use std::ffi::c_void;
use std::ptr::NonNull;

use frankenterm_alloc::resource_ledger::{GpuResourceGuard, GpuResourceLedger, GpuTexturePurpose};
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::sel;
use objc2_foundation::ns_string;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLResidencySet,
    MTLResidencySetDescriptor, MTLSize, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};

use crate::atlas::{
    AtlasChange, AtlasConfig, AtlasConfigError, AtlasFull, AtlasKind, AtlasSlot, FrameFence,
    GlyphAtlas, RetireQueue, texture_bytes,
};
use crate::{FrameError, MetalDevice};

/// Why a glyph did not reach an atlas.
#[derive(Debug)]
pub enum AtlasError {
    Config(AtlasConfigError),
    Full(AtlasFull),
    /// `pixels` was not exactly `width * height * bytes_per_pixel` bytes.
    PixelBytes {
        expected: usize,
        actual: usize,
    },
    Gpu(FrameError),
}

impl std::fmt::Display for AtlasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => write!(f, "atlas configuration: {error}"),
            Self::Full(full) => write!(f, "atlas full: {full:?}"),
            Self::PixelBytes { expected, actual } => {
                write!(f, "glyph pixels are {actual} bytes, expected {expected}")
            }
            Self::Gpu(error) => write!(f, "atlas GPU operation failed: {error}"),
        }
    }
}

impl std::error::Error for AtlasError {}

/// One atlas texture and its ledger entry, which is released with it.
struct AtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    _ledger: GpuResourceGuard,
}

/// One atlas: the allocator and the texture it packs.
struct MetalAtlas {
    atlas: GlyphAtlas,
    pixel_format: MTLPixelFormat,
    current: AtlasTexture,
    retired: RetireQueue<AtlasTexture>,
}

impl MetalAtlas {
    fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        kind: AtlasKind,
        config: AtlasConfig,
        pixel_format: MTLPixelFormat,
        ledger: &'static GpuResourceLedger,
    ) -> Result<Self, AtlasError> {
        let atlas = GlyphAtlas::new(kind, config).map_err(AtlasError::Config)?;
        let (width, height) = atlas.extent();
        let current = new_texture(device, kind, pixel_format, width, height, ledger)?;
        Ok(Self {
            atlas,
            pixel_format,
            current,
            retired: RetireQueue::default(),
        })
    }

    /// Replaces the texture with a taller one holding the same rows.
    fn grow(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        queue: &ProtocolObject<dyn MTLCommandQueue>,
        old_height: u32,
        fence: FrameFence,
        residency: Option<&ProtocolObject<dyn MTLResidencySet>>,
        ledger: &'static GpuResourceLedger,
    ) -> Result<(), AtlasError> {
        let kind = self.atlas.kind();
        let (width, height) = self.atlas.extent();
        let grown = new_texture(device, kind, self.pixel_format, width, height, ledger)?;
        let commands = queue
            .commandBuffer()
            .ok_or(AtlasError::Gpu(FrameError::CommandBufferUnavailable))?;
        let blit = commands
            .blitCommandEncoder()
            .ok_or(AtlasError::Gpu(FrameError::EncoderUnavailable))?;
        let origin = MTLOrigin { x: 0, y: 0, z: 0 };
        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. The source region is the old texture's whole
        // width x old_height x 1 extent in slice 0, level 0 (the old texture
        // is exactly that tall); the destination has the same width and is
        // taller, so the same region at the same origin fits. Both textures
        // are retained here until the command buffer has completed below.
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                &self.current.texture,
                0,
                0,
                origin,
                MTLSize {
                    width: width as usize,
                    height: old_height as usize,
                    depth: 1,
                },
                &grown.texture,
                0,
                0,
                origin,
            );
        }
        blit.endEncoding();
        commands.commit();
        // Growth happens a few times at most, during warm-up. Waiting here
        // orders the copy before any queue (Metal 3 or Metal 4) samples the
        // new texture, and before new glyphs are written into it.
        commands.waitUntilCompleted();
        let status = commands.status();
        if status != MTLCommandBufferStatus::Completed {
            let detail = commands.error().map_or_else(
                || format!("status {}", status.0),
                |error| error.localizedDescription().to_string(),
            );
            return Err(AtlasError::Gpu(FrameError::CommandFailed { detail }));
        }
        if let Some(set) = residency {
            set.addAllocation(ProtocolObject::from_ref(&*grown.texture));
            set.commit();
        }
        let old = std::mem::replace(&mut self.current, grown);
        // Frames up to the one being encoded may have sampled the old texture.
        self.retired.retire(old, fence.current);
        Ok(())
    }

    /// Writes a glyph's tightly packed pixels into its freshly allocated
    /// region.
    fn upload(&self, slot: &AtlasSlot, pixels: &[u8]) {
        let bytes_per_pixel = self.atlas.kind().bytes_per_pixel() as usize;
        let bytes_per_row = slot.width as usize * bytes_per_pixel;
        let region = MTLRegion {
            origin: MTLOrigin {
                x: slot.x as usize,
                y: slot.y as usize,
                z: 0,
            },
            size: MTLSize {
                width: slot.width as usize,
                height: slot.height as usize,
                depth: 1,
            },
        };
        let Some(bytes) = NonNull::new(pixels.as_ptr().cast_mut().cast::<c_void>()) else {
            return;
        };
        #[allow(unsafe_code)]
        // SAFETY: ATLAS-UPLOAD. The region comes from this atlas's allocator,
        // so it lies inside the texture, and it is either never-used space or
        // an evicted page whose last frame has finished: no unfinished frame
        // reads it. `pixels` was checked to hold exactly width * height *
        // bytes-per-pixel bytes, rows tightly packed at `bytes_per_row`, and
        // the borrow keeps it alive for the call. The texture is in shared
        // storage, which the CPU may write.
        unsafe {
            self.current
                .texture
                .replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, bytes, bytes_per_row);
        }
    }
}

/// A texture of `kind` with its ledger entry.
fn new_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    kind: AtlasKind,
    pixel_format: MTLPixelFormat,
    width: u32,
    height: u32,
    ledger: &'static GpuResourceLedger,
) -> Result<AtlasTexture, AtlasError> {
    if !(1..=crate::MAX_TEXTURE_EXTENT).contains(&width)
        || !(1..=crate::MAX_TEXTURE_EXTENT).contains(&height)
    {
        return Err(AtlasError::Gpu(FrameError::InvalidExtent { width, height }));
    }
    #[allow(unsafe_code)]
    // SAFETY: FFI-EXTENT. R8Unorm and BGRA8Unorm(_sRGB) are ordinary 2D
    // formats and the extent was checked to be within 1..=MAX_TEXTURE_EXTENT
    // on both axes above.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            pixel_format,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::ShaderRead);
    descriptor.setStorageMode(MTLStorageMode::Shared);
    let bytes = texture_bytes(kind, width, height);
    let texture = device
        .newTextureWithDescriptor(&descriptor)
        .ok_or(AtlasError::Gpu(FrameError::AllocationFailed {
            what: "glyph atlas texture",
            bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
        }))?;
    Ok(AtlasTexture {
        texture,
        _ledger: ledger.track_texture(GpuTexturePurpose::Atlas, bytes),
    })
}

/// The grayscale and color glyph atlases.
pub struct GlyphAtlases {
    atlases: [MetalAtlas; 2],
    residency: Option<Retained<ProtocolObject<dyn MTLResidencySet>>>,
    ledger: &'static GpuResourceLedger,
}

impl GlyphAtlases {
    /// Both atlases at their full initial size, in one residency set.
    /// `srgb` selects `BGRA8Unorm_sRGB` for the color atlas.
    pub fn new(
        device: &MetalDevice,
        grayscale: AtlasConfig,
        color: AtlasConfig,
        srgb: bool,
        ledger: &'static GpuResourceLedger,
    ) -> Result<Self, AtlasError> {
        let device = device.raw_device();
        let color_format = if srgb {
            MTLPixelFormat::BGRA8Unorm_sRGB
        } else {
            MTLPixelFormat::BGRA8Unorm
        };
        let atlases = [
            MetalAtlas::new(
                &device,
                AtlasKind::Grayscale,
                grayscale,
                MTLPixelFormat::R8Unorm,
                ledger,
            )?,
            MetalAtlas::new(&device, AtlasKind::Color, color, color_format, ledger)?,
        ];
        let residency = atlas_residency_set(&device);
        if let Some(set) = &residency {
            for atlas in &atlases {
                set.addAllocation(ProtocolObject::from_ref(&*atlas.current.texture));
            }
            set.commit();
            set.requestResidency();
        }
        Ok(Self {
            atlases,
            residency,
            ledger,
        })
    }

    fn atlas(&self, kind: AtlasKind) -> &MetalAtlas {
        &self.atlases[kind_index(kind)]
    }

    /// Places a `width x height` glyph of `kind` for the frame
    /// `fence.current` and uploads its pixels: rows of
    /// `width * bytes_per_pixel` bytes, tightly packed (one coverage byte
    /// per pixel for grayscale, `[B, G, R, A]` for color).
    pub fn insert(
        &mut self,
        device: &MetalDevice,
        kind: AtlasKind,
        width: u32,
        height: u32,
        pixels: &[u8],
        fence: FrameFence,
    ) -> Result<AtlasSlot, AtlasError> {
        let (queue, device) = (device.raw_queue(), device.raw_device());
        let expected = width as usize * height as usize * kind.bytes_per_pixel() as usize;
        if pixels.len() != expected {
            return Err(AtlasError::PixelBytes {
                expected,
                actual: pixels.len(),
            });
        }
        let ledger = self.ledger;
        let residency = self.residency.as_deref();
        let atlas = &mut self.atlases[kind_index(kind)];
        let allocation = atlas
            .atlas
            .allocate(width, height, fence)
            .map_err(AtlasError::Full)?;
        match allocation.change {
            Some(AtlasChange::Grew { old_height, .. }) => {
                ledger.record_atlas_generation();
                atlas.grow(&device, queue, old_height, fence, residency, ledger)?;
            }
            Some(AtlasChange::Evicted { .. }) => ledger.record_atlas_generation(),
            None => {}
        }
        atlas.upload(&allocation.slot, pixels);
        Ok(allocation.slot)
    }

    /// Records that the frame `frame` draws the glyph in `slot`; false when
    /// the slot is stale and the glyph must be inserted again.
    pub fn touch(&mut self, slot: &AtlasSlot, frame: u64) -> bool {
        self.atlases[kind_index(slot.kind)].atlas.touch(slot, frame)
    }

    #[must_use]
    pub fn is_live(&self, slot: &AtlasSlot) -> bool {
        self.atlas(slot.kind).atlas.is_live(slot)
    }

    /// Releases retired textures no unfinished frame can read; returns how
    /// many were released.
    pub fn collect_retired(&mut self, retired_before: u64) -> usize {
        let mut released = 0;
        for atlas in &mut self.atlases {
            let ready = atlas.retired.take_ready(retired_before);
            if let Some(set) = &self.residency {
                for retired in &ready {
                    set.removeAllocation(ProtocolObject::from_ref(&*retired.texture));
                }
                if !ready.is_empty() {
                    set.commit();
                }
            }
            released += ready.len();
        }
        released
    }

    /// The texture the glyph draw samples for `kind`; first bound by the
    /// glyph draw (ft-yccm0.4.2.3).
    #[allow(dead_code)]
    pub(crate) fn texture(&self, kind: AtlasKind) -> &ProtocolObject<dyn MTLTexture> {
        &self.atlas(kind).current.texture
    }

    /// The allocator state of `kind`'s atlas.
    #[must_use]
    pub fn allocator(&self, kind: AtlasKind) -> &GlyphAtlas {
        &self.atlas(kind).atlas
    }

    /// The atlases' residency set, if the OS has them (macOS 15+); the glyph
    /// draw (ft-yccm0.4.2.3) adds it to its queue.
    #[allow(dead_code)]
    pub(crate) fn residency(&self) -> Option<&ProtocolObject<dyn MTLResidencySet>> {
        self.residency.as_deref()
    }

    /// Retired textures still waiting for their frames to finish.
    #[must_use]
    pub fn retired(&self) -> usize {
        self.atlases.iter().map(|atlas| atlas.retired.len()).sum()
    }
}

/// The atlases' own residency set, or `None` before macOS 15 (no
/// `MTLResidencySet`). The same probe the frame slots use for theirs.
fn atlas_residency_set(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Option<Retained<ProtocolObject<dyn MTLResidencySet>>> {
    if !device.respondsToSelector(sel!(newResidencySetWithDescriptor:error:)) {
        return None;
    }
    let descriptor = MTLResidencySetDescriptor::new();
    descriptor.setLabel(Some(ns_string!("frankenterm glyph atlases")));
    device.newResidencySetWithDescriptor_error(&descriptor).ok()
}

fn kind_index(kind: AtlasKind) -> usize {
    match kind {
        AtlasKind::Grayscale => 0,
        AtlasKind::Color => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_metal::{MTLBuffer, MTLResourceOptions};

    fn device() -> MetalDevice {
        MetalDevice::system_default()
            .unwrap_or_else(|reason| panic!("this test needs an admitted Metal device: {reason}"))
    }

    fn ledger() -> &'static GpuResourceLedger {
        Box::leak(Box::new(GpuResourceLedger::new()))
    }

    fn small(kind: AtlasKind, max_pages: u32) -> AtlasConfig {
        AtlasConfig {
            width: 256,
            initial_height: 128,
            page_height: 64,
            padding: 1,
            max_bytes: texture_bytes(kind, 256, 64 * max_pages),
        }
    }

    /// Copies `slot`'s pixels out of the atlas texture.
    fn read_back(device: &MetalDevice, atlases: &GlyphAtlases, slot: &AtlasSlot) -> Vec<u8> {
        let bytes_per_pixel = slot.kind.bytes_per_pixel() as usize;
        let bytes_per_row = slot.width as usize * bytes_per_pixel;
        let len = bytes_per_row * slot.height as usize;
        let buffer = device
            .raw_device()
            .newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared)
            .unwrap();
        let commands = device.raw_queue().commandBuffer().unwrap();
        let blit = commands.blitCommandEncoder().unwrap();
        #[allow(unsafe_code)]
        // SAFETY: FFI-EXTENT. The source region is the slot's own region,
        // inside the texture; the destination pitch is the slot's width times
        // the format's bytes per pixel and `buffer` holds exactly pitch *
        // height bytes from offset 0.
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                atlases.texture(slot.kind),
                0,
                0,
                MTLOrigin {
                    x: slot.x as usize,
                    y: slot.y as usize,
                    z: 0,
                },
                MTLSize {
                    width: slot.width as usize,
                    height: slot.height as usize,
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
        assert_eq!(commands.status(), MTLCommandBufferStatus::Completed);
        assert!(buffer.length() >= len);
        let contents = buffer.contents().cast::<u8>();
        #[allow(unsafe_code)]
        // SAFETY: BUFFER-CONTENTS. `contents` covers `buffer.length()` bytes,
        // at least `len`; the command buffer that wrote them has completed, and
        // the bytes are copied out while `buffer` is alive.
        unsafe { std::slice::from_raw_parts(contents.as_ptr(), len) }.to_vec()
    }

    fn glyph(kind: AtlasKind, width: u32, height: u32, seed: u8) -> Vec<u8> {
        let len = width as usize * height as usize * kind.bytes_per_pixel() as usize;
        (0..len)
            .map(|index| seed.wrapping_add(u8::try_from(index % 251).unwrap()))
            .collect()
    }

    fn fence(current: u64) -> FrameFence {
        FrameFence {
            current,
            retired_before: current,
        }
    }

    #[test]
    fn both_atlases_start_at_their_full_size_in_the_ledger() {
        let device = device();
        let ledger = ledger();
        let atlases = GlyphAtlases::new(
            &device,
            AtlasConfig::for_kind(AtlasKind::Grayscale),
            AtlasConfig::for_kind(AtlasKind::Color),
            false,
            ledger,
        )
        .unwrap();
        for kind in AtlasKind::ALL {
            let texture = atlases.texture(kind);
            assert_eq!((texture.width(), texture.height()), (2048, 2048));
        }
        assert_eq!(
            atlases.texture(AtlasKind::Grayscale).pixelFormat(),
            MTLPixelFormat::R8Unorm
        );
        assert_eq!(
            atlases.texture(AtlasKind::Color).pixelFormat(),
            MTLPixelFormat::BGRA8Unorm
        );
        let counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(counter.live_count, 2);
        assert_eq!(counter.live_bytes, 2048 * 2048 * (1 + 4));
        if let Some(set) = atlases.residency() {
            assert_eq!(set.allocationCount(), 2);
        }
    }

    #[test]
    fn uploaded_glyphs_read_back_exactly_and_survive_growth() {
        let device = device();
        let ledger = ledger();
        let mut atlases = GlyphAtlases::new(
            &device,
            small(AtlasKind::Grayscale, 2),
            small(AtlasKind::Color, 4),
            true,
            ledger,
        )
        .unwrap();
        let mut placed = Vec::new();
        // 63x63 color glyphs (64x64 with padding): four per page, two pages
        // to start, so the ninth grows the color atlas to four pages.
        for index in 0..10_u8 {
            let pixels = glyph(AtlasKind::Color, 63, 63, index);
            let slot = atlases
                .insert(
                    &device,
                    AtlasKind::Color,
                    63,
                    63,
                    &pixels,
                    fence(u64::from(index)),
                )
                .unwrap();
            placed.push((slot, pixels));
        }
        assert_eq!(atlases.texture(AtlasKind::Color).height(), 256);
        assert_eq!(atlases.allocator(AtlasKind::Color).generation(), 1);
        assert_eq!(
            atlases.retired(),
            1,
            "the 128-row texture waits for frame 8"
        );
        for (slot, pixels) in &placed {
            assert!(atlases.is_live(slot));
            assert_eq!(&read_back(&device, &atlases, slot), pixels, "{slot:?}");
        }
        let gray = glyph(AtlasKind::Grayscale, 20, 30, 7);
        let slot = atlases
            .insert(&device, AtlasKind::Grayscale, 20, 30, &gray, fence(10))
            .unwrap();
        assert_eq!(read_back(&device, &atlases, &slot), gray);
        assert!(matches!(
            atlases.insert(&device, AtlasKind::Grayscale, 20, 30, &gray[1..], fence(10)),
            Err(AtlasError::PixelBytes { .. })
        ));
        // The retired texture leaves the ledger once frame 8 has finished.
        let counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(counter.live_count, 3);
        assert_eq!(atlases.collect_retired(8), 0);
        assert_eq!(atlases.collect_retired(9), 1);
        let counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(counter.live_count, 2);
        assert_eq!(counter.live_bytes, 256 * 128 + 256 * 256 * 4);
    }

    /// ft-yccm0.4.3.2 AC: after 1,000 forced evictions the ledger shows the
    /// same two atlas textures and bytes as before; nothing was recreated.
    #[test]
    fn a_thousand_forced_evictions_leave_the_atlas_ledger_flat() {
        let device = device();
        let ledger = ledger();
        let mut atlases = GlyphAtlases::new(
            &device,
            small(AtlasKind::Grayscale, 2),
            small(AtlasKind::Color, 2),
            false,
            ledger,
        )
        .unwrap();
        let before = ledger.texture_counter(GpuTexturePurpose::Atlas);
        // A full-page glyph: from the third on, every insert evicts a page.
        let pixels = glyph(AtlasKind::Color, 255, 63, 3);
        for frame in 0..1_002_u64 {
            let slot = atlases
                .insert(&device, AtlasKind::Color, 255, 63, &pixels, fence(frame))
                .unwrap();
            assert!(atlases.touch(&slot, frame));
            atlases.collect_retired(frame);
        }
        assert_eq!(atlases.allocator(AtlasKind::Color).stats().evictions, 1_000);
        let after = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(after.live_count, before.live_count);
        assert_eq!(after.live_bytes, before.live_bytes);
        assert_eq!(
            after.created_total, before.created_total,
            "no texture was recreated"
        );
        assert_eq!(ledger.snapshot().atlas_generations, 1_000);
    }
}
