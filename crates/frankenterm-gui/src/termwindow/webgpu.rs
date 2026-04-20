use crate::quad::Vertex;
use anyhow::anyhow;
use config::{ConfigHandle, GpuInfo, WebGpuPowerPreference};
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
        copy_padded_readback_to_image, initial_surface_extent, padded_readback_bytes_per_row,
        resize_surface_extent, select_composite_alpha_mode, select_surface_format,
        select_surface_view_formats, select_view_formats_for_format, wait_for_webgpu_readback_map,
    };
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use window::Dimensions;
    use window::bitmaps::{BitmapImage, Image};

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
