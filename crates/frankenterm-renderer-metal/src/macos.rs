//! The macOS implementation. Every `unsafe` block in the crate is here, and
//! each names its UNSAFE-CONTRACT category from the crate docs.

use crate::{
    ClearColor, DeviceCapabilities, FrameError, FrameOutcome, MAX_TEXTURE_EXTENT,
    MetalUnavailable, appkit_view,
};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{MainThreadMarker, msg_send};
use objc2_core_foundation::CGSize;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLGPUFamily,
    MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLRenderPassDescriptor, MTLResourceOptions,
    MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CALayer, CAMetalDrawable, CAMetalLayer};
use raw_window_handle::HasWindowHandle;
use std::cell::Cell;
use std::fmt;

/// The pixel format of every target this crate renders to.
const PIXEL_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
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

    /// Clears an offscreen `width x height` `BGRA8Unorm` texture with one
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
        // SAFETY: FFI-EXTENT. BGRA8Unorm is color-renderable and the extent
        // was checked to be within 1..=MAX_TEXTURE_EXTENT on both axes above.
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
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or(FrameError::AllocationFailed {
                what: "render target texture",
                bytes: len,
            })?;
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
}

impl fmt::Debug for MetalRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetalRenderer")
            .field("device", &self.device)
            .field("drawable_size", &self.drawable_size.get())
            .finish_non_exhaustive()
    }
}

impl MetalRenderer {
    /// Opens and admits the system default device, then binds it to the
    /// window's backing `CAMetalLayer`. Must be called on the main thread.
    ///
    /// The device is probed before the layer is touched, so a refusal leaves
    /// the layer exactly as the window created it for the fallback backend.
    pub fn attach(window: &impl HasWindowHandle) -> Result<Self, MetalUnavailable> {
        let device = MetalDevice::system_default()?;
        let handle = window
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
        layer.setDevice(Some(&device.device));
        layer.setPixelFormat(PIXEL_FORMAT);
        layer.setFramebufferOnly(true);
        Ok(Self {
            device,
            layer,
            drawable_size: Cell::new((0, 0)),
        })
    }

    /// The admitted device this renderer draws with.
    #[must_use]
    pub fn device(&self) -> &MetalDevice {
        &self.device
    }

    /// Clears the next drawable of a `width x height` pixel layer to `color`,
    /// schedules its presentation and commits. Does not wait for the GPU.
    pub fn render_clear(
        &self,
        width: u32,
        height: u32,
        color: ClearColor,
    ) -> Result<FrameOutcome, FrameError> {
        if width == 0 || height == 0 {
            return Ok(FrameOutcome::ZeroSize);
        }
        if self.drawable_size.get() != (width, height) {
            self.layer
                .setDrawableSize(CGSize::new(f64::from(width), f64::from(height)));
            self.drawable_size.set((width, height));
        }
        let drawable = self
            .layer
            .nextDrawable()
            .ok_or(FrameError::DrawableUnavailable)?;
        let commands = self
            .device
            .queue
            .commandBuffer()
            .ok_or(FrameError::CommandBufferUnavailable)?;
        let pass = clear_pass(&drawable.texture(), color);
        let encoder = commands
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or(FrameError::EncoderUnavailable)?;
        encoder.endEncoding();
        commands.presentDrawable(ProtocolObject::from_ref(&*drawable));
        commands.commit();
        Ok(FrameOutcome::Presented)
    }
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
    attachment.setClearColor(MTLClearColor {
        red: color.red,
        green: color.green,
        blue: color.blue,
        alpha: color.alpha,
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
        assert_eq!(MTLGPUFamily(GpuFamily::Apple(1).raw()), MTLGPUFamily::Apple1);
        assert_eq!(MTLGPUFamily(GpuFamily::Apple(7).raw()), MTLGPUFamily::Apple7);
        assert_eq!(MTLGPUFamily(GpuFamily::Apple(10).raw()), MTLGPUFamily::Apple10);
        assert_eq!(MTLGPUFamily(GpuFamily::Mac2.raw()), MTLGPUFamily::Mac2);
        assert_eq!(MTLGPUFamily(GpuFamily::Metal3.raw()), MTLGPUFamily::Metal3);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn apple_silicon_default_device_is_admitted() {
        let device = device();
        let caps = device.capabilities();
        assert!(
            caps.apple_family.is_some_and(|family| family >= crate::MIN_APPLE_FAMILY),
            "{caps}"
        );
        assert!(caps.unified_memory, "{caps}");
        assert_ne!(caps.name, "");
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
            assert_eq!(bytes.len(), width as usize * height as usize * BYTES_PER_PIXEL);
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
        let bytes = device.clear_offscreen(4, 4, color).expect("offscreen clear");
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
