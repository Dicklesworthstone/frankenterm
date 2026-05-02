use crate::quad::Vertex;
use anyhow::anyhow;
use config::{ConfigHandle, GpuInfo, WebGpuPowerPreference};
use frankenterm_core::color_management::{SurfaceFormatGamut, SurfaceGamutClassification};
use frankenterm_core::display_pipeline::{
    PresentAction, ScanoutBlockReason, ScanoutEligibility, VrrMechanism, VrrPlatform,
    WaylandCompositor, X11WindowManager, decide_present, negotiate_vrr_support,
    should_force_present,
};
use frankenterm_core::display_platform_probe::{DisplayProbeResult, PlatformOs};
use frankenterm_core::gpu_pipeline_cache::{
    PipelineCacheVersion, cache_filename, derive_cache_key,
};
use frankenterm_core::sparse_texture_atlas::{
    GpuVendor, ResolvedSparseDecision, SparseDecision, SparseFeatureQuery, SparseOverride,
    should_enable_sparse,
};
use frankenterm_core::wayland_direct_scanout::{
    BufferFormat as DirectScanoutBufferFormat, DirectScanoutDecision,
    ScanoutFallback as DirectScanoutFallback, ScanoutInputs as DirectScanoutInputs,
    ScanoutSupport as DirectScanoutSupport, WaylandCompositor as DirectScanoutCompositor,
    evaluate_direct_scanout,
};
use std::cell::RefCell;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use wgpu::util::DeviceExt;
use window::bitmaps::{Texture2d, validate_texture_readback_request};
use window::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use window::{BitmapImage, Dimensions, Rect, Window};

const WEBGPU_READBACK_TIMEOUT: Duration = Duration::from_secs(5);
const WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL: &str = "wgpu-25.0.2";
const WEBGPU_SHADER_SOURCE: &str = include_str!("../shader.wgsl");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebGpuDisplaySession {
    Wayland {
        compositor: Option<WaylandCompositor>,
    },
    X11 {
        window_manager: Option<X11WindowManager>,
        present_extension_available: bool,
    },
    Native,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct WebGpuPresentProbeInputs<'a> {
    pub probe: &'a DisplayProbeResult,
    pub session: WebGpuDisplaySession,
    pub scanout: ScanoutEligibility,
    pub dedup_says_skip: bool,
    pub a11y_query_in_flight: bool,
    pub manual_flush_requested: bool,
    pub post_layout_change: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebGpuTearingFreeMode {
    Vsync,
    ImmediateTearingAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebGpuTearingFreeReason {
    InputLatencyCritical,
    NoRecentInput,
    UnsupportedPlatform,
    CompositorUnsupported,
    XPresentMissing,
    MacOsAlwaysVsync,
    ForcedPresent,
    DedupSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebGpuTearingFreeDecision {
    pub action: PresentAction,
    pub mode: WebGpuTearingFreeMode,
    pub reason: WebGpuTearingFreeReason,
}

impl WebGpuTearingFreeDecision {
    #[must_use]
    pub fn uses_immediate_present(self) -> bool {
        self.mode == WebGpuTearingFreeMode::ImmediateTearingAllowed
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WebGpuTearingFreeInputs<'a> {
    pub present: WebGpuPresentProbeInputs<'a>,
    pub input_event_age: Option<Duration>,
    pub latency_critical_window: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct WebGpuDirectScanoutInputs<'a> {
    pub probe: &'a DisplayProbeResult,
    pub session: WebGpuDisplaySession,
    pub support: DirectScanoutSupport,
    pub fullscreen: bool,
    pub cursor_overlay_required: bool,
    pub partial_occlusion: bool,
    pub driver_known_broken: bool,
    pub compositor_advertised: &'a [DirectScanoutBufferFormat],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebGpuDirectScanoutDecision {
    pub direct_scanout: DirectScanoutDecision,
    pub present_scanout: ScanoutEligibility,
}

impl WebGpuDirectScanoutDecision {
    #[must_use]
    pub fn should_submit_dmabuf(self) -> bool {
        matches!(self.direct_scanout, DirectScanoutDecision::Active { .. })
            && self.present_scanout.is_eligible()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebGpuPipelineCacheProbe {
    pub version: PipelineCacheVersion,
    pub cache_key: u64,
    pub cache_filename: String,
    pub arch_label: String,
    pub wgpu_version_label: &'static str,
    pub driver_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebGpuSparseAtlasStartupProbe {
    pub vendor: GpuVendor,
    pub query: SparseFeatureQuery,
    pub override_: SparseOverride,
    pub decision: ResolvedSparseDecision,
}

#[must_use]
pub fn decide_webgpu_present_from_probe(inputs: WebGpuPresentProbeInputs<'_>) -> PresentAction {
    let (platform, wayland_compositor, x11_present_available) =
        webgpu_vrr_probe_inputs(inputs.probe.platform, inputs.session);
    let vrr = negotiate_vrr_support(platform, wayland_compositor, x11_present_available);
    let force_present = should_force_present(
        inputs.probe.recording.forces_present(),
        inputs.a11y_query_in_flight,
        inputs.manual_flush_requested,
        inputs.post_layout_change,
    );

    decide_present(vrr, inputs.scanout, inputs.dedup_says_skip, force_present)
}

#[must_use]
pub fn decide_webgpu_tearing_free_present(
    inputs: WebGpuTearingFreeInputs<'_>,
) -> WebGpuTearingFreeDecision {
    let action = decide_webgpu_present_from_probe(inputs.present);
    match action {
        PresentAction::Skip => {
            return WebGpuTearingFreeDecision {
                action,
                mode: WebGpuTearingFreeMode::Vsync,
                reason: WebGpuTearingFreeReason::DedupSkipped,
            };
        }
        PresentAction::ForcePresent { .. } => {
            return WebGpuTearingFreeDecision {
                action,
                mode: WebGpuTearingFreeMode::Vsync,
                reason: WebGpuTearingFreeReason::ForcedPresent,
            };
        }
        PresentAction::PresentNow { .. } => {}
    }

    let latency_critical = inputs
        .input_event_age
        .is_some_and(|age| age <= inputs.latency_critical_window);
    if !latency_critical {
        return WebGpuTearingFreeDecision {
            action,
            mode: WebGpuTearingFreeMode::Vsync,
            reason: WebGpuTearingFreeReason::NoRecentInput,
        };
    }

    let PresentAction::PresentNow { mechanism, .. } = action else {
        unreachable!("early return above handles non-PresentNow actions");
    };

    match (
        inputs.present.probe.platform,
        inputs.present.session,
        mechanism,
    ) {
        (
            PlatformOs::Linux,
            WebGpuDisplaySession::Wayland { .. },
            VrrMechanism::WpTearingControlV1,
        )
        | (PlatformOs::Linux, WebGpuDisplaySession::X11 { .. }, VrrMechanism::XPresentAsync) => {
            WebGpuTearingFreeDecision {
                action,
                mode: WebGpuTearingFreeMode::ImmediateTearingAllowed,
                reason: WebGpuTearingFreeReason::InputLatencyCritical,
            }
        }
        (PlatformOs::MacOs, _, _) => WebGpuTearingFreeDecision {
            action,
            mode: WebGpuTearingFreeMode::Vsync,
            reason: WebGpuTearingFreeReason::MacOsAlwaysVsync,
        },
        (
            PlatformOs::Linux,
            WebGpuDisplaySession::X11 {
                present_extension_available: false,
                ..
            },
            VrrMechanism::FixedRate,
        ) => WebGpuTearingFreeDecision {
            action,
            mode: WebGpuTearingFreeMode::Vsync,
            reason: WebGpuTearingFreeReason::XPresentMissing,
        },
        (PlatformOs::Linux, WebGpuDisplaySession::Wayland { .. }, VrrMechanism::FixedRate) => {
            WebGpuTearingFreeDecision {
                action,
                mode: WebGpuTearingFreeMode::Vsync,
                reason: WebGpuTearingFreeReason::CompositorUnsupported,
            }
        }
        _ => WebGpuTearingFreeDecision {
            action,
            mode: WebGpuTearingFreeMode::Vsync,
            reason: WebGpuTearingFreeReason::UnsupportedPlatform,
        },
    }
}

#[must_use]
pub fn decide_webgpu_direct_scanout(
    inputs: WebGpuDirectScanoutInputs<'_>,
) -> WebGpuDirectScanoutDecision {
    let compositor = webgpu_direct_scanout_compositor(inputs.probe.platform, inputs.session);
    let support = if compositor.is_some() {
        inputs.support
    } else {
        DirectScanoutSupport::Unknown
    };
    let direct_inputs = DirectScanoutInputs {
        compositor: compositor.unwrap_or(DirectScanoutCompositor::Sway),
        support,
        fullscreen: inputs.fullscreen,
        cursor_overlay_required: inputs.cursor_overlay_required,
        partial_occlusion: inputs.partial_occlusion,
        driver_known_broken: inputs.driver_known_broken,
        compositor_advertised: inputs.compositor_advertised.to_vec(),
    };
    let direct_scanout = evaluate_direct_scanout(&direct_inputs);
    let present_scanout = webgpu_present_scanout_eligibility(inputs.probe, direct_scanout);

    WebGpuDirectScanoutDecision {
        direct_scanout,
        present_scanout,
    }
}

#[must_use]
fn webgpu_vrr_probe_inputs(
    platform: PlatformOs,
    session: WebGpuDisplaySession,
) -> (VrrPlatform, Option<WaylandCompositor>, bool) {
    match platform {
        PlatformOs::MacOs => (VrrPlatform::MacOs, None, false),
        PlatformOs::Windows => (VrrPlatform::Windows, None, false),
        PlatformOs::Linux => match session {
            WebGpuDisplaySession::Wayland { compositor } => {
                (VrrPlatform::Wayland, compositor, false)
            }
            WebGpuDisplaySession::X11 {
                present_extension_available,
                ..
            } => (VrrPlatform::X11, None, present_extension_available),
            WebGpuDisplaySession::Native | WebGpuDisplaySession::Unknown => {
                (VrrPlatform::Wayland, None, false)
            }
        },
        PlatformOs::Other => (VrrPlatform::Wayland, None, false),
    }
}

#[must_use]
fn webgpu_direct_scanout_compositor(
    platform: PlatformOs,
    session: WebGpuDisplaySession,
) -> Option<DirectScanoutCompositor> {
    if platform != PlatformOs::Linux {
        return None;
    }

    match session {
        WebGpuDisplaySession::Wayland {
            compositor: Some(WaylandCompositor::Mutter),
        } => Some(DirectScanoutCompositor::Mutter),
        WebGpuDisplaySession::Wayland {
            compositor: Some(WaylandCompositor::Kwin),
        } => Some(DirectScanoutCompositor::Kwin),
        WebGpuDisplaySession::Wayland {
            compositor: Some(WaylandCompositor::Sway),
        } => Some(DirectScanoutCompositor::Sway),
        WebGpuDisplaySession::Wayland {
            compositor: Some(WaylandCompositor::Hyprland),
        } => Some(DirectScanoutCompositor::Hyprland),
        WebGpuDisplaySession::Wayland { .. }
        | WebGpuDisplaySession::X11 { .. }
        | WebGpuDisplaySession::Native
        | WebGpuDisplaySession::Unknown => None,
    }
}

#[must_use]
fn webgpu_present_scanout_eligibility(
    probe: &DisplayProbeResult,
    direct_scanout: DirectScanoutDecision,
) -> ScanoutEligibility {
    if probe.recording.forces_present() {
        return ScanoutEligibility::Blocked(ScanoutBlockReason::RecordingActive);
    }

    match direct_scanout {
        DirectScanoutDecision::Active { .. } => ScanoutEligibility::Eligible,
        DirectScanoutDecision::Fallback { cause } => ScanoutEligibility::Blocked(match cause {
            DirectScanoutFallback::NotFullscreen => ScanoutBlockReason::NotFullscreen,
            DirectScanoutFallback::CompositorUnsupported => ScanoutBlockReason::CompositorNoDmabuf,
            DirectScanoutFallback::FormatNegotiationFailed => {
                ScanoutBlockReason::BufferFormatNotSupported
            }
            DirectScanoutFallback::DriverBug => ScanoutBlockReason::VendorModifierNotAdvertised,
            DirectScanoutFallback::CursorOverlay | DirectScanoutFallback::PartialOcclusion => {
                ScanoutBlockReason::MultiPaneOverlayPresent
            }
        }),
    }
}

#[must_use]
pub fn build_webgpu_pipeline_cache_probe(
    adapter_info: &wgpu::AdapterInfo,
    ft_binary_hash: u64,
) -> WebGpuPipelineCacheProbe {
    let arch_label = webgpu_pipeline_cache_arch_label();
    let driver_label = webgpu_pipeline_cache_driver_label(adapter_info);
    let version = PipelineCacheVersion::new(
        webgpu_pipeline_cache_hash(&arch_label),
        webgpu_pipeline_cache_hash(WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL),
        webgpu_pipeline_cache_hash(&driver_label),
        webgpu_pipeline_cache_hash(WEBGPU_SHADER_SOURCE),
        ft_binary_hash,
    );
    let cache_key = derive_cache_key(version);
    let cache_filename = cache_filename(&arch_label, WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL);

    WebGpuPipelineCacheProbe {
        version,
        cache_key,
        cache_filename,
        arch_label,
        wgpu_version_label: WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL,
        driver_label,
    }
}

#[must_use]
fn webgpu_pipeline_cache_arch_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[must_use]
fn webgpu_pipeline_cache_driver_label(adapter_info: &wgpu::AdapterInfo) -> String {
    let driver_info = if adapter_info.driver_info.is_empty() {
        "unknown"
    } else {
        adapter_info.driver_info.as_str()
    };
    let driver = if adapter_info.driver.is_empty() {
        "unknown"
    } else {
        adapter_info.driver.as_str()
    };

    format!(
        "{:?}|{}|{}|vendor={:04x}|device={:04x}|{}",
        adapter_info.backend,
        driver,
        driver_info,
        adapter_info.vendor,
        adapter_info.device,
        adapter_info.name,
    )
}

#[must_use]
fn webgpu_pipeline_cache_hash(input: &str) -> u64 {
    const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME_64: u64 = 0x100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS_64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

#[repr(C)]
#[derive(Copy, Clone, Default, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShaderUniform {
    pub foreground_text_hsb: [f32; 3],
    pub milliseconds: u32,
    pub projection: [[f32; 4]; 4],
    // sampler2D atlas_nearest_sampler;
    // sampler2D atlas_linear_sampler;
}

pub struct WebGpuState {
    pub adapter_info: wgpu::AdapterInfo,
    pub downlevel_caps: wgpu::DownlevelCapabilities,
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: Arc<wgpu::Queue>,
    pub config: RefCell<wgpu::SurfaceConfiguration>,
    pub dimensions: RefCell<Dimensions>,
    pub render_pipeline: wgpu::RenderPipeline,
    shader_uniform_buffer: wgpu::Buffer,
    shader_uniform_bind_group: wgpu::BindGroup,
    #[allow(dead_code)]
    shader_uniform_bind_group_layout: wgpu::BindGroupLayout,
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    pub texture_nearest_sampler: wgpu::Sampler,
    pub texture_linear_sampler: wgpu::Sampler,
    pub handle: RawHandlePair,
}

pub struct RawHandlePair {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

impl RawHandlePair {
    fn new(window: &Window) -> Self {
        Self {
            window: window.window_handle().expect("window handle").as_raw(),
            display: window.display_handle().expect("display handle").as_raw(),
        }
    }
}

impl HasWindowHandle for RawHandlePair {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        unsafe { Ok(WindowHandle::borrow_raw(self.window)) }
    }
}

impl HasDisplayHandle for RawHandlePair {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        unsafe { Ok(DisplayHandle::borrow_raw(self.display)) }
    }
}

pub struct WebGpuTexture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
    device: wgpu::Device,
    queue: Arc<wgpu::Queue>,
}

impl std::ops::Deref for WebGpuTexture {
    type Target = wgpu::Texture;
    fn deref(&self) -> &Self::Target {
        &self.texture
    }
}

impl Texture2d for WebGpuTexture {
    fn write(&self, rect: Rect, im: &dyn BitmapImage) {
        let (im_width, im_height) = im.image_dimensions();

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.min_x() as u32,
                    y: rect.min_y() as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            im.pixel_data_slice(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(im_width as u32 * 4),
                rows_per_image: Some(im_height as u32),
            },
            wgpu::Extent3d {
                width: im_width as u32,
                height: im_height as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    fn read(&self, rect: Rect, im: &mut dyn BitmapImage) -> anyhow::Result<()> {
        let request = validate_texture_readback_request(self.width(), self.height(), rect, im)?;
        if request.width == 0 || request.height == 0 {
            return Ok(());
        }

        let bytes_per_row = padded_readback_bytes_per_row(request.width);
        let buffer_size = bytes_per_row as u64 * request.height as u64;
        let readback_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("WebGpuTexture readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("WebGpuTexture readback encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: request.left,
                    y: request.top,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(request.height),
                },
            },
            wgpu::Extent3d {
                width: request.width,
                height: request.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback_buffer.slice(..buffer_size);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });

        // wgpu 25 removed caller-specified timeouts from PollType::Wait, so
        // preserve FrankenTerm's 5-second readback deadline with a bounded poll loop.
        wait_for_webgpu_readback_map(
            WEBGPU_READBACK_TIMEOUT,
            Duration::from_millis(10),
            || {
                self.device
                    .poll(wgpu::PollType::Poll)
                    .map_err(|err| anyhow!("polling webgpu readback failed: {err:?}"))?;
                Ok(())
            },
            |poll_interval| receiver.recv_timeout(poll_interval),
            Instant::now,
        )?;

        let data = slice.get_mapped_range();
        // wgpu requires 256-byte row alignment for copy-to-buffer, but the
        // BitmapImage contract is tightly packed RGBA bytes.
        copy_padded_readback_to_image(&data, bytes_per_row as usize, im);
        drop(data);
        readback_buffer.unmap();
        Ok(())
    }

    fn width(&self) -> usize {
        self.width as usize
    }

    fn height(&self) -> usize {
        self.height as usize
    }
}

impl WebGpuTexture {
    pub fn new(width: u32, height: u32, state: &WebGpuState) -> anyhow::Result<Self> {
        let limit = state.device.limits().max_texture_dimension_2d;

        if width > limit || height > limit {
            // Ideally, wgpu would have a fallible create_texture method,
            // but it doesn't: instead it will panic if the requested
            // dimension is too large.
            // So we check the limit ourselves here.
            // <https://github.com/wezterm/wezterm/issues/3713>
            anyhow::bail!(
                "texture dimensions {width}x{height} exceed the \
                 max dimension {limit} supported by your GPU"
            );
        }

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let view_formats = if state
            .downlevel_caps
            .flags
            .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS)
        {
            select_view_formats_for_format(format)
        } else {
            vec![]
        };
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            label: Some("Texture Atlas"),
            view_formats: &view_formats,
        });
        Ok(Self {
            texture,
            width,
            height,
            device: state.device.clone(),
            queue: Arc::clone(&state.queue),
        })
    }
}

/// Compute the aligned row pitch required by wgpu copy-to-buffer readback for
/// tightly packed RGBA8 pixels.
fn padded_readback_bytes_per_row(width: u32) -> u32 {
    let unpadded = width.saturating_mul(4);
    let alignment = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    if unpadded == 0 {
        0
    } else {
        let aligned = u64::from(unpadded).div_ceil(alignment) * alignment;
        let max_aligned = (u64::from(u32::MAX) / alignment) * alignment;
        aligned.min(max_aligned) as u32
    }
}

fn wait_for_webgpu_readback_map<E, TPoll, TRecv, TNow>(
    timeout: Duration,
    poll_interval: Duration,
    mut poll: TPoll,
    mut recv: TRecv,
    mut now: TNow,
) -> anyhow::Result<()>
where
    E: std::fmt::Debug,
    TPoll: FnMut() -> anyhow::Result<()>,
    TRecv: FnMut(Duration) -> Result<Result<(), E>, mpsc::RecvTimeoutError>,
    TNow: FnMut() -> Instant,
{
    let start = now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    loop {
        poll()?;

        match recv(poll_interval) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                return Err(anyhow!("mapping webgpu readback buffer failed: {err:?}"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if now() < deadline => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(anyhow!("timed out waiting for webgpu readback mapping"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!(
                    "webgpu readback mapping callback disconnected before completion"
                ));
            }
        }
    }
}

/// Strip per-row readback padding and populate the destination image with
/// tightly packed RGBA bytes.
fn copy_padded_readback_to_image(
    padded_data: &[u8],
    padded_bytes_per_row: usize,
    dest: &mut dyn BitmapImage,
) {
    let (width, height) = dest.image_dimensions();
    let unpadded_bytes_per_row = width * 4;

    for row in 0..height {
        let src_offset = row * padded_bytes_per_row;
        let dst_offset = row * unpadded_bytes_per_row;
        dest.pixel_data_slice_mut()[dst_offset..dst_offset + unpadded_bytes_per_row]
            .copy_from_slice(&padded_data[src_offset..src_offset + unpadded_bytes_per_row]);
    }
}

pub fn adapter_info_to_gpu_info(info: wgpu::AdapterInfo) -> GpuInfo {
    GpuInfo {
        name: info.name,
        vendor: Some(info.vendor),
        device: Some(info.device),
        device_type: format!("{:?}", info.device_type),
        driver: if info.driver.is_empty() {
            None
        } else {
            Some(info.driver)
        },
        driver_info: if info.driver_info.is_empty() {
            None
        } else {
            Some(info.driver_info)
        },
        backend: format!("{:?}", info.backend),
    }
}

#[must_use]
pub fn classify_sparse_atlas_vendor(info: &wgpu::AdapterInfo) -> GpuVendor {
    let blob = sparse_adapter_info_blob(info);
    match info.vendor {
        0x10de => GpuVendor::Nvidia,
        0x1002 | 0x1022 => GpuVendor::AmdGcn1Plus,
        0x106b => GpuVendor::AppleSilicon,
        0x8086 => classify_intel_sparse_vendor(&blob),
        _ if blob.contains("apple m") || blob.contains("apple gpu") || blob.contains("agx") => {
            GpuVendor::AppleSilicon
        }
        _ if blob.contains("nvidia") || blob.contains("geforce") || blob.contains("rtx") => {
            GpuVendor::Nvidia
        }
        _ if blob.contains("amd") || blob.contains("radeon") => GpuVendor::AmdGcn1Plus,
        _ if blob.contains("intel") => classify_intel_sparse_vendor(&blob),
        _ => GpuVendor::Other,
    }
}

fn classify_intel_sparse_vendor(blob: &str) -> GpuVendor {
    if blob.contains("tiger lake")
        || blob.contains("iris xe")
        || blob.contains("xe graphics")
        || blob.contains("arc")
        || blob.contains("alder lake")
        || blob.contains("raptor lake")
        || blob.contains("meteor lake")
        || blob.contains("lunar lake")
        || blob.contains("battle mage")
    {
        GpuVendor::IntelTigerLakePlus
    } else {
        GpuVendor::IntelOlder
    }
}

fn sparse_adapter_info_blob(info: &wgpu::AdapterInfo) -> String {
    format!(
        "{} {} {} {:?} {:04x} {:04x}",
        info.name, info.driver, info.driver_info, info.backend, info.vendor, info.device,
    )
    .to_ascii_lowercase()
}

#[must_use]
pub fn sparse_feature_query_from_limits(limits: &wgpu::Limits) -> SparseFeatureQuery {
    SparseFeatureQuery {
        // wgpu 25 exposes texture-array limits, but not a portable
        // sparse-residency capability bit or tile-commit API. Keep
        // Auto conservative so the substrate routes to the flat atlas
        // until a backend-specific sparse path is actually available.
        sparse_residency_available: false,
        texture_array_available: limits.max_texture_array_layers > 1,
        max_array_layers: limits.max_texture_array_layers,
        max_texture_2d_dim: limits.max_texture_dimension_2d,
    }
}

#[must_use]
pub fn build_sparse_atlas_startup_probe(
    info: &wgpu::AdapterInfo,
    limits: &wgpu::Limits,
    override_: SparseOverride,
) -> WebGpuSparseAtlasStartupProbe {
    let vendor = classify_sparse_atlas_vendor(info);
    let query = sparse_feature_query_from_limits(limits);
    let decision = should_enable_sparse(vendor, query, override_);
    WebGpuSparseAtlasStartupProbe {
        vendor,
        query,
        override_,
        decision,
    }
}

fn compute_compatibility_list(
    instance: &wgpu::Instance,
    backends: wgpu::Backends,
    surface: &wgpu::Surface,
) -> Vec<String> {
    instance
        .enumerate_adapters(backends)
        .into_iter()
        .map(|a| {
            let info = adapter_info_to_gpu_info(a.get_info());
            let compatible = a.is_surface_supported(&surface);
            format!(
                "{}, compatible={}",
                info.to_string(),
                if compatible { "yes" } else { "NO" }
            )
        })
        .collect()
}

fn select_surface_format(formats: &[wgpu::TextureFormat]) -> anyhow::Result<wgpu::TextureFormat> {
    let first = formats
        .first()
        .copied()
        .ok_or_else(|| anyhow!("surface capability format list should not be empty"))?;
    let preferred_srgb = first.add_srgb_suffix();
    Ok(if formats.contains(&preferred_srgb) {
        preferred_srgb
    } else {
        first
    })
}

/// Classify the chosen surface format's color space (ft-mpc9b.10.3).
///
/// This is the **observability** half of the color-management
/// foundation: the per-display ICC integration beads under
/// `ft-mpc9b.10` will swap the renderer's color path; until then,
/// emitting the gamut at startup gives the fixture in
/// `crates/frankenterm-core/tests/color_regression_fixture.rs`
/// (and any future telemetry consumer) a stable observation point.
///
/// The classifier mirrors `wgpu::TextureFormat`'s `Debug`
/// representation by name to avoid taking a `wgpu` dependency from
/// `frankenterm-core` — the table lives at
/// `crate::color_management::SurfaceFormatGamut`.
fn classify_surface_color_space(format: wgpu::TextureFormat) -> SurfaceGamutClassification {
    SurfaceFormatGamut::classify(&format!("{format:?}"))
}

fn select_view_formats_for_format(format: wgpu::TextureFormat) -> Vec<wgpu::TextureFormat> {
    let srgb = format.add_srgb_suffix();
    let linear = format.remove_srgb_suffix();
    if srgb == linear {
        vec![format]
    } else {
        vec![srgb, linear]
    }
}

fn select_surface_view_formats(
    format: wgpu::TextureFormat,
    downlevel_caps: &wgpu::DownlevelCapabilities,
) -> Vec<wgpu::TextureFormat> {
    if downlevel_caps
        .flags
        .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS)
    {
        select_view_formats_for_format(format)
    } else {
        vec![]
    }
}

fn clamp_surface_dimension_for_configuration(value: usize) -> u32 {
    value.max(1).min(u32::MAX as usize) as u32
}

fn initial_surface_extent(dimensions: Dimensions) -> (u32, u32) {
    (
        clamp_surface_dimension_for_configuration(dimensions.pixel_width),
        clamp_surface_dimension_for_configuration(dimensions.pixel_height),
    )
}

fn resize_surface_extent(dimensions: Dimensions) -> (u32, u32) {
    (
        dimensions.pixel_width.min(u32::MAX as usize) as u32,
        dimensions.pixel_height.min(u32::MAX as usize) as u32,
    )
}

fn select_composite_alpha_mode(
    alpha_modes: &[wgpu::CompositeAlphaMode],
) -> wgpu::CompositeAlphaMode {
    if alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
        wgpu::CompositeAlphaMode::PostMultiplied
    } else if alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    }
}

impl WebGpuState {
    pub async fn new(
        window: &Window,
        dimensions: Dimensions,
        config: &ConfigHandle,
    ) -> anyhow::Result<Self> {
        let handle = RawHandlePair::new(window);
        Self::new_impl(handle, dimensions, config).await
    }

    pub async fn new_impl(
        handle: RawHandlePair,
        dimensions: Dimensions,
        config: &ConfigHandle,
    ) -> anyhow::Result<Self> {
        let backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&handle)?)?
        };

        let mut adapter: Option<wgpu::Adapter> = None;

        if let Some(preference) = &config.webgpu_preferred_adapter {
            for a in instance.enumerate_adapters(backends) {
                if !a.is_surface_supported(&surface) {
                    let info = adapter_info_to_gpu_info(a.get_info());
                    log::warn!("{} is not compatible with surface", info.to_string());
                    continue;
                }

                let info = a.get_info();

                if preference.name != info.name {
                    continue;
                }

                if preference.device_type != format!("{:?}", info.device_type) {
                    continue;
                }

                if preference.backend != format!("{:?}", info.backend) {
                    continue;
                }

                if let Some(driver) = &preference.driver {
                    if *driver != info.driver {
                        continue;
                    }
                }
                if let Some(vendor) = &preference.vendor {
                    if *vendor != info.vendor {
                        continue;
                    }
                }
                if let Some(device) = &preference.device {
                    if *device != info.device {
                        continue;
                    }
                }

                adapter.replace(a);
                break;
            }

            if adapter.is_none() {
                let adapters = compute_compatibility_list(&instance, backends, &surface);
                log::warn!(
                    "Your webgpu preferred adapter '{}' was either not \
                     found or is not compatible with your display. Available:\n{}",
                    preference.to_string(),
                    adapters.join("\n")
                );
            }
        }

        if adapter.is_none() {
            adapter = Some(
                instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: match config.webgpu_power_preference {
                            WebGpuPowerPreference::HighPerformance => {
                                wgpu::PowerPreference::HighPerformance
                            }
                            WebGpuPowerPreference::LowPower => wgpu::PowerPreference::LowPower,
                        },
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: config.webgpu_force_fallback_adapter,
                    })
                    .await?,
            );
        }

        let adapter = adapter.ok_or_else(|| {
            let adapters = compute_compatibility_list(&instance, backends, &surface);
            anyhow!(
                "no compatible adapter found. Available:\n{}",
                adapters.join("\n")
            )
        })?;

        let adapter_info = adapter.get_info();
        log::trace!("Using adapter: {adapter_info:?}");
        let pipeline_cache_probe = build_webgpu_pipeline_cache_probe(&adapter_info, 0);
        let sparse_atlas_probe = build_sparse_atlas_startup_probe(
            &adapter_info,
            &adapter.limits(),
            SparseOverride::Auto,
        );
        log::info!(
            "webgpu pipeline cache probe: key={:016x} file={} arch={} wgpu={} driver={}",
            pipeline_cache_probe.cache_key,
            pipeline_cache_probe.cache_filename,
            pipeline_cache_probe.arch_label,
            pipeline_cache_probe.wgpu_version_label,
            pipeline_cache_probe.driver_label,
        );
        log::info!(
            "webgpu sparse atlas probe: vendor={:?} sparse={} texture_array={} max_layers={} \
             max_texture_2d={} override={} decision={:?} reason={:?}",
            sparse_atlas_probe.vendor,
            sparse_atlas_probe.query.sparse_residency_available,
            sparse_atlas_probe.query.texture_array_available,
            sparse_atlas_probe.query.max_array_layers,
            sparse_atlas_probe.query.max_texture_2d_dim,
            sparse_atlas_probe.override_,
            sparse_atlas_probe.decision.decision,
            sparse_atlas_probe.decision.reason,
        );
        if matches!(
            sparse_atlas_probe.decision.decision,
            SparseDecision::Fallback
        ) {
            log::debug!("webgpu sparse atlas inactive; using flat 2D glyph atlas");
        }
        let caps = surface.get_capabilities(&adapter);
        log::trace!("caps: {caps:?}");
        let downlevel_caps = adapter.get_downlevel_capabilities();
        log::trace!("downlevel_caps: {downlevel_caps:?}");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features: wgpu::Features::empty(),
                // WebGL doesn't support all of wgpu's features, so if
                // we're building for the web we'll have to disable some.
                required_limits: if cfg!(target_arch = "wasm32") {
                    wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    wgpu::Limits::downlevel_defaults()
                }
                .using_resolution(adapter.limits()),
                label: None,
                memory_hints: Default::default(),
                trace: wgpu::Trace::Off,
            })
            .await?;

        let queue = Arc::new(queue);

        let format = select_surface_format(&caps.formats)?;
        // Need to check that this is supported, as trying to set
        // view_formats without it will cause surface.configure
        // to panic
        // <https://github.com/wezterm/wezterm/issues/3565>
        let view_formats = select_surface_view_formats(format, &downlevel_caps);
        let (surface_width, surface_height) = initial_surface_extent(dimensions);

        // ft-mpc9b.10.3: log the chosen surface's color space at
        // startup. Until the per-display ICC integration beads land,
        // wide-gamut formats are reported with `wide_gamut_unverified`
        // so consumers know the gamut is the modal expected value
        // rather than a measured one.
        let gamut = classify_surface_color_space(format);
        log::info!(
            "webgpu surface configured: format={format:?} \
             color_space={:?} wide_gamut_unverified={} hdr_capable={}",
            gamut.color_space,
            gamut.wide_gamut_unverified,
            gamut.hdr_capable,
        );

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: surface_width,
            height: surface_height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: select_composite_alpha_mode(&caps.alpha_modes),
            view_formats,
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::include_wgsl!("../shader.wgsl"));

        let shader_uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("ShaderUniform bind group layout"),
            });
        let shader_uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("ShaderUniform Buffer"),
            contents: bytemuck::bytes_of(&ShaderUniform::default()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let shader_uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shader_uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: shader_uniform_buffer.as_entire_binding(),
            }],
            label: Some("ShaderUniform Bind Group"),
        });

        let texture_nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let texture_linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
                label: Some("texture bind group layout"),
            });

        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Render Pipeline Layout"),
                bind_group_layouts: &[
                    &shader_uniform_bind_group_layout,
                    &texture_bind_group_layout,
                    &texture_bind_group_layout,
                ],
                push_constant_ranges: &[],
            });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::desc()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),

            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        Ok(Self {
            adapter_info,
            downlevel_caps,
            surface,
            device,
            queue,
            config: RefCell::new(config),
            dimensions: RefCell::new(dimensions),
            render_pipeline,
            shader_uniform_buffer,
            shader_uniform_bind_group,
            handle,
            shader_uniform_bind_group_layout,
            texture_bind_group_layout,
            texture_nearest_sampler,
            texture_linear_sampler,
        })
    }

    pub fn update_uniform(&self, uniform: ShaderUniform) {
        self.queue
            .write_buffer(&self.shader_uniform_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    pub fn shader_uniform_bind_group(&self) -> &wgpu::BindGroup {
        &self.shader_uniform_bind_group
    }

    #[allow(unused_mut)]
    pub fn resize(&self, mut dims: Dimensions) {
        // During a live resize on Windows, the Dimensions that we're processing may be
        // lagging behind the true client size. We have to take the very latest value
        // from the window or else the underlying driver will raise an error about
        // the mismatch, so we need to sneakily read through the handle
        match self.handle.window {
            #[cfg(windows)]
            RawWindowHandle::Win32(h) => {
                let mut rect = unsafe { std::mem::zeroed() };
                unsafe { winapi::um::winuser::GetClientRect(h.hwnd.get() as _, &mut rect) };
                dims.pixel_width = (rect.right - rect.left) as usize;
                dims.pixel_height = (rect.bottom - rect.top) as usize;
            }
            _ => {}
        }

        if dims == *self.dimensions.borrow() {
            return;
        }
        *self.dimensions.borrow_mut() = dims;
        let mut config = self.config.borrow_mut();
        let (width, height) = resize_surface_extent(dims);
        config.width = width;
        config.height = height;
        if config.width > 0 && config.height > 0 {
            // Avoid reconfiguring with a 0 sized surface, as webgpu will
            // panic in that case
            // <https://github.com/wezterm/wezterm/issues/2881>
            self.surface.configure(&self.device, &config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL, WEBGPU_SHADER_SOURCE, WebGpuDirectScanoutInputs,
        WebGpuDisplaySession, WebGpuPresentProbeInputs, WebGpuTearingFreeInputs,
        WebGpuTearingFreeMode, WebGpuTearingFreeReason, build_sparse_atlas_startup_probe,
        build_webgpu_pipeline_cache_probe, classify_sparse_atlas_vendor,
        classify_surface_color_space, copy_padded_readback_to_image, decide_webgpu_direct_scanout,
        decide_webgpu_present_from_probe, decide_webgpu_tearing_free_present,
        initial_surface_extent, padded_readback_bytes_per_row, resize_surface_extent,
        select_composite_alpha_mode, select_surface_format, select_surface_view_formats,
        select_view_formats_for_format, wait_for_webgpu_readback_map,
        webgpu_direct_scanout_compositor, webgpu_pipeline_cache_hash, webgpu_vrr_probe_inputs,
    };
    use anyhow::anyhow;
    use frankenterm_core::display_pipeline::{
        PresentAction, ScanoutBlockReason, ScanoutEligibility, VrrMechanism, VrrPlatform,
        WaylandCompositor, X11WindowManager,
    };
    use frankenterm_core::display_platform_probe::{
        DisplayProbeResult, PlatformOs, ProbeConfidence, RecordingProbe, RecordingState,
        RefreshRangeProbe, VrrProbe, VrrSupportLevel,
    };
    use frankenterm_core::sparse_texture_atlas::{
        GpuVendor, SparseDecision, SparseFeatureQuery, SparseOverride,
    };
    use frankenterm_core::wayland_direct_scanout::{
        BufferFormat as DirectScanoutBufferFormat, DirectScanoutDecision,
        ScanoutFallback as DirectScanoutFallback, ScanoutSupport as DirectScanoutSupport,
        WaylandCompositor as DirectScanoutCompositor,
    };
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use window::Dimensions;
    use window::bitmaps::{BitmapImage, Image};

    fn probe(platform: PlatformOs, recording: RecordingState) -> DisplayProbeResult {
        DisplayProbeResult {
            platform,
            refresh: RefreshRangeProbe {
                max_hz: 144,
                min_hz: 48,
                preferred_hz: 120,
                confidence: ProbeConfidence::Authoritative,
            },
            vrr: VrrProbe {
                level: VrrSupportLevel::EnabledAndAvailable,
                confidence: ProbeConfidence::Authoritative,
            },
            recording: RecordingProbe {
                state: recording,
                confidence: ProbeConfidence::Authoritative,
            },
            probe_succeeded: true,
            probe_skipped_reason: None,
        }
    }

    fn present_inputs(
        probe: &DisplayProbeResult,
        session: WebGpuDisplaySession,
    ) -> WebGpuPresentProbeInputs<'_> {
        WebGpuPresentProbeInputs {
            probe,
            session,
            scanout: ScanoutEligibility::Eligible,
            dedup_says_skip: false,
            a11y_query_in_flight: false,
            manual_flush_requested: false,
            post_layout_change: false,
        }
    }

    fn tearing_free_decision(
        present: WebGpuPresentProbeInputs<'_>,
        input_event_age: Option<Duration>,
    ) -> super::WebGpuTearingFreeDecision {
        decide_webgpu_tearing_free_present(WebGpuTearingFreeInputs {
            present,
            input_event_age,
            latency_critical_window: Duration::from_millis(16),
        })
    }

    fn adapter_info(driver: &str, driver_info: &str) -> wgpu::AdapterInfo {
        wgpu::AdapterInfo {
            name: "Test GPU".to_string(),
            vendor: 0x10de,
            device: 0x2684,
            device_type: wgpu::DeviceType::DiscreteGpu,
            driver: driver.to_string(),
            driver_info: driver_info.to_string(),
            backend: wgpu::Backend::Vulkan,
        }
    }

    #[allow(dead_code)]
    #[derive(Debug)]
    struct ReadbackWaitSnapshot {
        result: String,
        poll_calls: usize,
        recv_timeouts_ms: Vec<u128>,
    }

    fn snapshot_readback_wait<E: std::fmt::Debug>(
        recv_results: impl IntoIterator<Item = Result<Result<(), E>, mpsc::RecvTimeoutError>>,
        now_offsets_ms: impl IntoIterator<Item = u64>,
    ) -> ReadbackWaitSnapshot {
        let timeout = Duration::from_millis(20);
        let poll_interval = Duration::from_millis(5);
        let base = Instant::now();
        let mut recv_results = recv_results.into_iter().collect::<VecDeque<_>>();
        let mut now_offsets = now_offsets_ms
            .into_iter()
            .map(|offset| base + Duration::from_millis(offset))
            .collect::<VecDeque<_>>();
        let mut last_now = base;
        let mut poll_calls = 0;
        let mut recv_timeouts_ms = Vec::new();
        let result = wait_for_webgpu_readback_map(
            timeout,
            poll_interval,
            || {
                poll_calls += 1;
                Ok(())
            },
            |timeout| {
                recv_timeouts_ms.push(timeout.as_millis());
                recv_results
                    .pop_front()
                    .expect("test must provide enough recv results")
            },
            || {
                let now = now_offsets.pop_front().unwrap_or(last_now);
                last_now = now;
                now
            },
        );

        ReadbackWaitSnapshot {
            result: result
                .map(|_| "ok".to_string())
                .unwrap_or_else(|err| err.to_string()),
            poll_calls,
            recv_timeouts_ms,
        }
    }

    #[test]
    fn webgpu_probe_maps_wayland_session_to_tearing_control() {
        let (platform, compositor, x11_present_available) = webgpu_vrr_probe_inputs(
            PlatformOs::Linux,
            WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Sway),
            },
        );

        assert_eq!(platform, VrrPlatform::Wayland);
        assert_eq!(compositor, Some(WaylandCompositor::Sway));
        assert!(!x11_present_available);
    }

    #[test]
    fn webgpu_probe_maps_x11_session_to_present_extension_flag() {
        let (platform, compositor, x11_present_available) = webgpu_vrr_probe_inputs(
            PlatformOs::Linux,
            WebGpuDisplaySession::X11 {
                window_manager: Some(X11WindowManager::Kwin),
                present_extension_available: true,
            },
        );

        assert_eq!(platform, VrrPlatform::X11);
        assert_eq!(compositor, None);
        assert!(x11_present_available);
    }

    #[test]
    fn webgpu_direct_scanout_maps_known_wayland_compositor() {
        let compositor = webgpu_direct_scanout_compositor(
            PlatformOs::Linux,
            WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Hyprland),
            },
        );

        assert_eq!(compositor, Some(DirectScanoutCompositor::Hyprland));
    }

    #[test]
    fn webgpu_direct_scanout_activates_when_wayland_gates_pass() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = decide_webgpu_direct_scanout(WebGpuDirectScanoutInputs {
            probe: &probe,
            session: WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Sway),
            },
            support: DirectScanoutSupport::Native,
            fullscreen: true,
            cursor_overlay_required: false,
            partial_occlusion: false,
            driver_known_broken: false,
            compositor_advertised: &[
                DirectScanoutBufferFormat::Rgba8,
                DirectScanoutBufferFormat::Bgra8,
            ],
        });

        assert_eq!(
            decision.direct_scanout,
            DirectScanoutDecision::Active {
                format: DirectScanoutBufferFormat::Bgra8
            }
        );
        assert_eq!(decision.present_scanout, ScanoutEligibility::Eligible);
        assert!(decision.should_submit_dmabuf());
    }

    #[test]
    fn webgpu_direct_scanout_blocks_dmabuf_when_recording_probe_is_unknown() {
        let probe = probe(PlatformOs::Linux, RecordingState::UnknownAssumeActive);
        let decision = decide_webgpu_direct_scanout(WebGpuDirectScanoutInputs {
            probe: &probe,
            session: WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Mutter),
            },
            support: DirectScanoutSupport::Native,
            fullscreen: true,
            cursor_overlay_required: false,
            partial_occlusion: false,
            driver_known_broken: false,
            compositor_advertised: &[DirectScanoutBufferFormat::Bgra8],
        });

        assert!(matches!(
            decision.direct_scanout,
            DirectScanoutDecision::Active { .. }
        ));
        assert_eq!(
            decision.present_scanout.block_reason(),
            Some(ScanoutBlockReason::RecordingActive)
        );
        assert!(!decision.should_submit_dmabuf());
    }

    #[test]
    fn webgpu_direct_scanout_maps_x11_to_compositor_fallback() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = decide_webgpu_direct_scanout(WebGpuDirectScanoutInputs {
            probe: &probe,
            session: WebGpuDisplaySession::X11 {
                window_manager: Some(X11WindowManager::I3),
                present_extension_available: true,
            },
            support: DirectScanoutSupport::Native,
            fullscreen: true,
            cursor_overlay_required: false,
            partial_occlusion: false,
            driver_known_broken: false,
            compositor_advertised: &[DirectScanoutBufferFormat::Bgra8],
        });

        assert_eq!(
            decision.direct_scanout,
            DirectScanoutDecision::Fallback {
                cause: DirectScanoutFallback::CompositorUnsupported
            }
        );
        assert_eq!(
            decision.present_scanout.block_reason(),
            Some(ScanoutBlockReason::CompositorNoDmabuf)
        );
        assert!(!decision.should_submit_dmabuf());
    }

    #[test]
    fn webgpu_direct_scanout_maps_cursor_overlay_to_present_overlay_block() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = decide_webgpu_direct_scanout(WebGpuDirectScanoutInputs {
            probe: &probe,
            session: WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Kwin),
            },
            support: DirectScanoutSupport::Native,
            fullscreen: true,
            cursor_overlay_required: true,
            partial_occlusion: false,
            driver_known_broken: false,
            compositor_advertised: &[DirectScanoutBufferFormat::Bgra8],
        });

        assert_eq!(
            decision.direct_scanout,
            DirectScanoutDecision::Fallback {
                cause: DirectScanoutFallback::CursorOverlay
            }
        );
        assert_eq!(
            decision.present_scanout.block_reason(),
            Some(ScanoutBlockReason::MultiPaneOverlayPresent)
        );
        assert!(!decision.should_submit_dmabuf());
    }

    #[test]
    fn webgpu_pipeline_cache_probe_uses_adapter_driver_and_shader_hash() {
        let adapter = adapter_info("nvidia", "535.154.05");
        let probe = build_webgpu_pipeline_cache_probe(&adapter, 0xfeed_cafe);

        assert_eq!(
            probe.wgpu_version_label,
            WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL
        );
        assert_eq!(
            probe.version.wgpu_version_hash,
            webgpu_pipeline_cache_hash(WEBGPU_PIPELINE_CACHE_WGPU_VERSION_LABEL)
        );
        assert_eq!(
            probe.version.shader_hash,
            webgpu_pipeline_cache_hash(WEBGPU_SHADER_SOURCE)
        );
        assert_eq!(probe.version.ft_binary_hash, 0xfeed_cafe);
        assert!(probe.driver_label.contains("nvidia"));
        assert!(probe.driver_label.contains("535.154.05"));
        assert!(probe.cache_filename.ends_with("-wgpu-25.0.2.bin"));
    }

    #[test]
    fn webgpu_pipeline_cache_probe_changes_when_driver_changes() {
        let old = build_webgpu_pipeline_cache_probe(&adapter_info("nvidia", "535.154.05"), 7);
        let new = build_webgpu_pipeline_cache_probe(&adapter_info("nvidia", "550.40.07"), 7);

        assert_ne!(
            old.version.driver_version_hash,
            new.version.driver_version_hash
        );
        assert_ne!(old.cache_key, new.cache_key);
    }

    #[test]
    fn webgpu_pipeline_cache_probe_uses_unknown_driver_fallback_label() {
        let probe = build_webgpu_pipeline_cache_probe(&adapter_info("", ""), 1);

        assert!(probe.driver_label.contains("unknown|unknown"));
        assert_ne!(probe.version.driver_version_hash, 0);
    }

    #[test]
    fn sparse_atlas_vendor_classifies_tier_one_pci_ids() {
        let mut nvidia = adapter_info("nvidia", "535.154.05");
        nvidia.vendor = 0x10de;
        assert_eq!(classify_sparse_atlas_vendor(&nvidia), GpuVendor::Nvidia);

        let mut amd = adapter_info("amd", "mesa radv");
        amd.vendor = 0x1002;
        assert_eq!(classify_sparse_atlas_vendor(&amd), GpuVendor::AmdGcn1Plus);

        let mut apple = adapter_info("metal", "apple");
        apple.vendor = 0x106b;
        assert_eq!(
            classify_sparse_atlas_vendor(&apple),
            GpuVendor::AppleSilicon
        );
    }

    #[test]
    fn sparse_atlas_vendor_splits_intel_generation_by_adapter_name() {
        let mut tiger_lake = adapter_info("mesa", "iris");
        tiger_lake.vendor = 0x8086;
        tiger_lake.name = "Intel Iris Xe Graphics".to_string();
        assert_eq!(
            classify_sparse_atlas_vendor(&tiger_lake),
            GpuVendor::IntelTigerLakePlus
        );

        let mut older = adapter_info("mesa", "i965");
        older.vendor = 0x8086;
        older.name = "Intel HD Graphics 4000".to_string();
        assert_eq!(classify_sparse_atlas_vendor(&older), GpuVendor::IntelOlder);
    }

    #[test]
    fn sparse_atlas_startup_probe_uses_flat_fallback_until_wgpu_exposes_sparse_residency() {
        let adapter = adapter_info("nvidia", "535.154.05");
        let limits = wgpu::Limits {
            max_texture_array_layers: 128,
            max_texture_dimension_2d: 8192,
            ..wgpu::Limits::downlevel_defaults()
        };
        let probe = build_sparse_atlas_startup_probe(&adapter, &limits, SparseOverride::Auto);

        assert_eq!(probe.vendor, GpuVendor::Nvidia);
        assert_eq!(
            probe.query,
            SparseFeatureQuery {
                sparse_residency_available: false,
                texture_array_available: true,
                max_array_layers: 128,
                max_texture_2d_dim: 8192,
            }
        );
        assert_eq!(probe.decision.decision, SparseDecision::Fallback);
        assert_eq!(
            probe.decision.reason,
            frankenterm_core::sparse_texture_atlas::SparseDecisionReason::FeatureQueryNegative
        );
    }

    #[test]
    fn sparse_atlas_startup_probe_honors_force_on_override() {
        let adapter = adapter_info("nvidia", "535.154.05");
        let probe = build_sparse_atlas_startup_probe(
            &adapter,
            &wgpu::Limits::downlevel_defaults(),
            SparseOverride::ForceOn,
        );

        assert_eq!(probe.decision.decision, SparseDecision::Native);
        assert_eq!(
            probe.decision.reason,
            frankenterm_core::sparse_texture_atlas::SparseDecisionReason::Override
        );
    }

    #[test]
    fn webgpu_present_probe_forces_present_when_recording_probe_is_unknown() {
        let probe = probe(PlatformOs::MacOs, RecordingState::UnknownAssumeActive);
        let action = decide_webgpu_present_from_probe(WebGpuPresentProbeInputs {
            probe: &probe,
            session: WebGpuDisplaySession::Native,
            scanout: ScanoutEligibility::Blocked(ScanoutBlockReason::NotFullscreen),
            dedup_says_skip: true,
            a11y_query_in_flight: false,
            manual_flush_requested: false,
            post_layout_change: false,
        });

        match action {
            PresentAction::ForcePresent { mechanism, reason } => {
                assert_eq!(mechanism, VrrMechanism::CaDisplayLinkDynamic);
                assert_eq!(
                    reason,
                    frankenterm_core::display_pipeline::ForcePresentReason::ScreenRecordingActive
                );
            }
            other => panic!("expected ForcePresent, got {other:?}"),
        }
    }

    #[test]
    fn webgpu_present_probe_allows_dedup_skip_when_recording_inactive() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let action = decide_webgpu_present_from_probe(WebGpuPresentProbeInputs {
            probe: &probe,
            session: WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Mutter),
            },
            scanout: ScanoutEligibility::Eligible,
            dedup_says_skip: true,
            a11y_query_in_flight: false,
            manual_flush_requested: false,
            post_layout_change: false,
        });

        assert_eq!(action, PresentAction::Skip);
    }

    #[test]
    fn webgpu_present_probe_presents_with_x11_mechanism_when_dedup_keeps_frame() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let action = decide_webgpu_present_from_probe(WebGpuPresentProbeInputs {
            probe: &probe,
            session: WebGpuDisplaySession::X11 {
                window_manager: Some(X11WindowManager::I3),
                present_extension_available: true,
            },
            scanout: ScanoutEligibility::Blocked(ScanoutBlockReason::NotFullscreen),
            dedup_says_skip: false,
            a11y_query_in_flight: false,
            manual_flush_requested: false,
            post_layout_change: false,
        });

        match action {
            PresentAction::PresentNow { mechanism, scanout } => {
                assert_eq!(mechanism, VrrMechanism::XPresentAsync);
                assert_eq!(
                    scanout.block_reason(),
                    Some(ScanoutBlockReason::NotFullscreen)
                );
            }
            other => panic!("expected PresentNow, got {other:?}"),
        }
    }

    #[test]
    fn tearing_free_present_uses_wayland_immediate_for_recent_input() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = tearing_free_decision(
            present_inputs(
                &probe,
                WebGpuDisplaySession::Wayland {
                    compositor: Some(WaylandCompositor::Sway),
                },
            ),
            Some(Duration::from_millis(4)),
        );

        assert!(decision.uses_immediate_present());
        assert_eq!(
            decision.mode,
            WebGpuTearingFreeMode::ImmediateTearingAllowed
        );
        assert_eq!(
            decision.reason,
            WebGpuTearingFreeReason::InputLatencyCritical
        );
        assert!(matches!(
            decision.action,
            PresentAction::PresentNow {
                mechanism: VrrMechanism::WpTearingControlV1,
                ..
            }
        ));
    }

    #[test]
    fn tearing_free_present_keeps_vsync_when_input_is_stale() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = tearing_free_decision(
            present_inputs(
                &probe,
                WebGpuDisplaySession::Wayland {
                    compositor: Some(WaylandCompositor::Sway),
                },
            ),
            Some(Duration::from_millis(17)),
        );

        assert!(!decision.uses_immediate_present());
        assert_eq!(decision.mode, WebGpuTearingFreeMode::Vsync);
        assert_eq!(decision.reason, WebGpuTearingFreeReason::NoRecentInput);
    }

    #[test]
    fn tearing_free_present_uses_xpresent_immediate_for_recent_input() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = tearing_free_decision(
            present_inputs(
                &probe,
                WebGpuDisplaySession::X11 {
                    window_manager: Some(X11WindowManager::Kwin),
                    present_extension_available: true,
                },
            ),
            Some(Duration::from_millis(1)),
        );

        assert!(decision.uses_immediate_present());
        assert_eq!(
            decision.reason,
            WebGpuTearingFreeReason::InputLatencyCritical
        );
        assert!(matches!(
            decision.action,
            PresentAction::PresentNow {
                mechanism: VrrMechanism::XPresentAsync,
                ..
            }
        ));
    }

    #[test]
    fn tearing_free_present_keeps_vsync_when_xpresent_is_missing() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let decision = tearing_free_decision(
            present_inputs(
                &probe,
                WebGpuDisplaySession::X11 {
                    window_manager: Some(X11WindowManager::Kwin),
                    present_extension_available: false,
                },
            ),
            Some(Duration::from_millis(1)),
        );

        assert!(!decision.uses_immediate_present());
        assert_eq!(decision.mode, WebGpuTearingFreeMode::Vsync);
        assert_eq!(decision.reason, WebGpuTearingFreeReason::XPresentMissing);
        assert!(matches!(
            decision.action,
            PresentAction::PresentNow {
                mechanism: VrrMechanism::FixedRate,
                ..
            }
        ));
    }

    #[test]
    fn tearing_free_present_keeps_macos_on_vsync() {
        let probe = probe(PlatformOs::MacOs, RecordingState::Inactive);
        let decision = tearing_free_decision(
            present_inputs(&probe, WebGpuDisplaySession::Native),
            Some(Duration::from_millis(1)),
        );

        assert!(!decision.uses_immediate_present());
        assert_eq!(decision.mode, WebGpuTearingFreeMode::Vsync);
        assert_eq!(decision.reason, WebGpuTearingFreeReason::MacOsAlwaysVsync);
        assert!(matches!(
            decision.action,
            PresentAction::PresentNow {
                mechanism: VrrMechanism::CaDisplayLinkDynamic,
                ..
            }
        ));
    }

    #[test]
    fn tearing_free_present_keeps_forced_present_on_vsync() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let mut present = present_inputs(
            &probe,
            WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Sway),
            },
        );
        present.manual_flush_requested = true;
        let decision = tearing_free_decision(present, Some(Duration::from_millis(1)));

        assert!(!decision.uses_immediate_present());
        assert_eq!(decision.mode, WebGpuTearingFreeMode::Vsync);
        assert_eq!(decision.reason, WebGpuTearingFreeReason::ForcedPresent);
        assert!(matches!(
            decision.action,
            PresentAction::ForcePresent { .. }
        ));
    }

    #[test]
    fn tearing_free_present_keeps_dedup_skip_on_vsync() {
        let probe = probe(PlatformOs::Linux, RecordingState::Inactive);
        let mut present = present_inputs(
            &probe,
            WebGpuDisplaySession::Wayland {
                compositor: Some(WaylandCompositor::Sway),
            },
        );
        present.dedup_says_skip = true;
        let decision = tearing_free_decision(present, Some(Duration::from_millis(1)));

        assert!(!decision.uses_immediate_present());
        assert_eq!(decision.mode, WebGpuTearingFreeMode::Vsync);
        assert_eq!(decision.reason, WebGpuTearingFreeReason::DedupSkipped);
        assert_eq!(decision.action, PresentAction::Skip);
    }

    #[test]
    fn surface_format_prefers_srgb_variant() {
        let formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        ];

        assert_eq!(
            select_surface_format(&formats).unwrap(),
            wgpu::TextureFormat::Bgra8UnormSrgb
        );
    }

    #[test]
    fn surface_format_uses_first_when_no_srgb_variant_exists() {
        let formats = [
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureFormat::Bgra8Unorm,
        ];

        assert_eq!(
            select_surface_format(&formats).unwrap(),
            wgpu::TextureFormat::Rgba16Float
        );
    }

    #[test]
    fn surface_format_keeps_first_family_even_if_later_entries_have_srgb_pairs() {
        let formats = [
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        ];

        assert_eq!(
            select_surface_format(&formats).unwrap(),
            wgpu::TextureFormat::Rgba16Float
        );
    }

    #[test]
    fn surface_format_rejects_empty_capabilities_list() {
        assert!(select_surface_format(&[]).is_err());
    }

    /// ft-mpc9b.10.3: the surface-format classifier picks the right
    /// `ColorSpace` for the formats `select_surface_format` actually
    /// chooses. Locks in the bridge between `wgpu::TextureFormat`'s
    /// `Debug` name and `frankenterm_core::color_management::SurfaceFormatGamut`.
    #[test]
    fn surface_color_space_classifier_recognizes_srgb_path() {
        use frankenterm_core::color_management::ColorSpace;
        let cls = classify_surface_color_space(wgpu::TextureFormat::Bgra8UnormSrgb);
        assert_eq!(cls.color_space, ColorSpace::Srgb);
        assert!(!cls.wide_gamut_unverified);
        assert!(!cls.hdr_capable);
    }

    #[test]
    fn surface_color_space_classifier_flags_wide_gamut_as_unverified() {
        use frankenterm_core::color_management::ColorSpace;
        let cls = classify_surface_color_space(wgpu::TextureFormat::Rgba16Float);
        assert_eq!(cls.color_space, ColorSpace::DisplayP3);
        assert!(cls.wide_gamut_unverified);
        assert!(cls.hdr_capable);
    }

    #[test]
    fn surface_color_space_classifier_handles_unknown_formats() {
        use frankenterm_core::color_management::ColorSpace;
        let cls = classify_surface_color_space(wgpu::TextureFormat::R8Sint);
        assert_eq!(cls.color_space, ColorSpace::Unknown);
    }

    #[test]
    fn surface_view_formats_require_support_flag() {
        let mut caps = wgpu::DownlevelCapabilities::default();
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;

        caps.flags
            .remove(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS);
        assert!(select_surface_view_formats(format, &caps).is_empty());

        caps.flags
            .insert(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS);
        assert_eq!(
            select_surface_view_formats(format, &caps),
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Bgra8Unorm,
            ]
        );
    }

    #[test]
    fn view_formats_deduplicate_when_format_has_no_srgb_pair() {
        assert_eq!(
            select_view_formats_for_format(wgpu::TextureFormat::Rgba16Float),
            vec![wgpu::TextureFormat::Rgba16Float]
        );
    }

    #[test]
    fn view_formats_normalize_linear_input_to_srgb_then_linear() {
        assert_eq!(
            select_view_formats_for_format(wgpu::TextureFormat::Bgra8Unorm),
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Bgra8Unorm,
            ]
        );
    }

    #[test]
    fn view_formats_normalize_srgb_input_to_srgb_then_linear() {
        assert_eq!(
            select_view_formats_for_format(wgpu::TextureFormat::Bgra8UnormSrgb),
            vec![
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Bgra8Unorm,
            ]
        );
    }

    #[test]
    fn surface_view_formats_deduplicate_non_pair_formats_even_with_support_flag() {
        let mut caps = wgpu::DownlevelCapabilities::default();
        caps.flags
            .insert(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS);

        assert_eq!(
            select_surface_view_formats(wgpu::TextureFormat::Rgba16Float, &caps),
            vec![wgpu::TextureFormat::Rgba16Float]
        );
    }

    #[test]
    fn alpha_mode_prefers_post_then_pre_then_auto() {
        assert_eq!(
            select_composite_alpha_mode(&[
                wgpu::CompositeAlphaMode::Opaque,
                wgpu::CompositeAlphaMode::PreMultiplied,
            ]),
            wgpu::CompositeAlphaMode::PreMultiplied
        );
        assert_eq!(
            select_composite_alpha_mode(&[
                wgpu::CompositeAlphaMode::Inherit,
                wgpu::CompositeAlphaMode::PostMultiplied,
            ]),
            wgpu::CompositeAlphaMode::PostMultiplied
        );
        assert_eq!(
            select_composite_alpha_mode(&[
                wgpu::CompositeAlphaMode::Opaque,
                wgpu::CompositeAlphaMode::Inherit,
            ]),
            wgpu::CompositeAlphaMode::Auto
        );
    }

    #[test]
    fn alpha_mode_defaults_to_auto_for_empty_capabilities() {
        assert_eq!(
            select_composite_alpha_mode(&[]),
            wgpu::CompositeAlphaMode::Auto
        );
    }

    #[test]
    fn initial_surface_extent_clamps_zero_dimensions_to_one() {
        assert_eq!(
            initial_surface_extent(Dimensions {
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            }),
            (1, 1)
        );
    }

    #[test]
    fn initial_surface_extent_preserves_non_zero_dimensions() {
        assert_eq!(
            initial_surface_extent(Dimensions {
                pixel_width: 1280,
                pixel_height: 720,
                dpi: 96,
            }),
            (1280, 720)
        );
    }

    #[test]
    fn initial_surface_extent_clamps_large_dimensions_to_u32_max() {
        assert_eq!(
            initial_surface_extent(Dimensions {
                pixel_width: usize::MAX,
                pixel_height: usize::MAX,
                dpi: 96,
            }),
            (u32::MAX, u32::MAX)
        );
    }

    #[test]
    fn initial_surface_extent_clamps_each_axis_independently() {
        assert_eq!(
            initial_surface_extent(Dimensions {
                pixel_width: 0,
                pixel_height: usize::MAX,
                dpi: 96,
            }),
            (1, u32::MAX)
        );
    }

    #[test]
    fn resize_surface_extent_preserves_zero_dimensions() {
        assert_eq!(
            resize_surface_extent(Dimensions {
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            }),
            (0, 0)
        );
    }

    #[test]
    fn resize_surface_extent_clamps_large_dimensions_to_u32_max() {
        assert_eq!(
            resize_surface_extent(Dimensions {
                pixel_width: usize::MAX,
                pixel_height: usize::MAX,
                dpi: 96,
            }),
            (u32::MAX, u32::MAX)
        );
    }

    #[test]
    fn resize_surface_extent_clamps_each_axis_independently() {
        assert_eq!(
            resize_surface_extent(Dimensions {
                pixel_width: 0,
                pixel_height: usize::MAX,
                dpi: 96,
            }),
            (0, u32::MAX)
        );
    }

    #[test]
    fn padded_readback_bytes_per_row_aligns_to_wgpu_requirement() {
        assert_eq!(padded_readback_bytes_per_row(0), 0);
        assert_eq!(padded_readback_bytes_per_row(1), 256);
        assert_eq!(padded_readback_bytes_per_row(64), 256);
        assert_eq!(padded_readback_bytes_per_row(65), 512);
    }

    #[test]
    fn padded_readback_bytes_per_row_saturates_at_largest_aligned_u32() {
        assert_eq!(
            padded_readback_bytes_per_row(u32::MAX),
            u32::MAX - (wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1)
        );
    }

    #[test]
    fn copy_padded_readback_to_image_strips_row_padding() {
        let mut dest = Image::new(2, 2);
        let padded = [
            1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0,
        ];

        copy_padded_readback_to_image(&padded, 32, &mut dest);

        assert_eq!(
            dest.pixel_data_slice(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn wait_for_webgpu_readback_map_retries_until_callback_completes() {
        k9::snapshot!(
            snapshot_readback_wait::<&'static str>(
                [Err(mpsc::RecvTimeoutError::Timeout), Ok(Ok(()))],
                [0, 5],
            ),
            "
ReadbackWaitSnapshot {
    result: \"ok\",
    poll_calls: 2,
    recv_timeouts_ms: [
        5,
        5,
    ],
}
"
        );
    }

    #[test]
    fn wait_for_webgpu_readback_map_times_out_at_deadline_boundary() {
        k9::snapshot!(
            snapshot_readback_wait::<&'static str>(
                [
                    Err(mpsc::RecvTimeoutError::Timeout),
                    Err(mpsc::RecvTimeoutError::Timeout),
                ],
                [0, 5, 20],
            ),
            "
ReadbackWaitSnapshot {
    result: \"timed out waiting for webgpu readback mapping\",
    poll_calls: 2,
    recv_timeouts_ms: [
        5,
        5,
    ],
}
"
        );
    }

    #[test]
    fn wait_for_webgpu_readback_map_reports_disconnected_callback() {
        k9::snapshot!(
            snapshot_readback_wait::<&'static str>(
                [Err(mpsc::RecvTimeoutError::Disconnected)],
                [0],
            ),
            "
ReadbackWaitSnapshot {
    result: \"webgpu readback mapping callback disconnected before completion\",
    poll_calls: 1,
    recv_timeouts_ms: [
        5,
    ],
}
"
        );
    }

    #[test]
    fn wait_for_webgpu_readback_map_propagates_callback_error() {
        k9::snapshot!(
            snapshot_readback_wait::<&'static str>([Ok(Err("device lost"))], [0],),
            "
ReadbackWaitSnapshot {
    result: \"mapping webgpu readback buffer failed: \\\"device lost\\\"\",
    poll_calls: 1,
    recv_timeouts_ms: [
        5,
    ],
}
"
        );
    }

    #[test]
    fn wait_for_webgpu_readback_map_propagates_poll_error_before_waiting_on_channel() {
        let mut recv_called = false;
        let result = wait_for_webgpu_readback_map::<&'static str, _, _, _>(
            Duration::from_millis(20),
            Duration::from_millis(5),
            || Err(anyhow!("readback cancelled")),
            |_| {
                recv_called = true;
                Err(mpsc::RecvTimeoutError::Disconnected)
            },
            Instant::now,
        );

        assert_eq!(result.unwrap_err().to_string(), "readback cancelled");
        assert!(
            !recv_called,
            "poll failure/cancellation should return before waiting on the callback channel"
        );
    }
}
