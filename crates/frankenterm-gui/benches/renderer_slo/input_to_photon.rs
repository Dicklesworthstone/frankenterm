//! Classified-input headless renderer proxy Criterion substrate.
//!
//! This bench intentionally writes a structured evidence row before Criterion
//! starts sampling. Criterion output proves timing regressions; the JSONL row
//! tells the release-attestation consumer whether the proxy was measured or
//! degraded. This substrate does not observe native input, mux/PTY traversal,
//! production-window presentation, scan-out, or photons.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_gui::glyph_quad_staging::{
    GlyphQuadSoaBuffers, GlyphQuadStagingInstance, GlyphQuadStagingVertex, VERTICES_PER_GLYPH_QUAD,
    aos_glyph_quad_vertices, moonshot_instanced_glyph_quads_enabled,
};
use frankenterm_gui::glyph_run_interning::{
    GlyphRunProbeGlyph, glyph_run_interning_enabled, glyph_run_probe_iteration,
};
use frankenterm_gui::headless_render::render_headless;
use frankenterm_gui::renderer_slo::headless::{
    classified_input_headless_fixture, trace_from_headless_frame,
};
use frankenterm_gui::renderer_slo::{
    INPUT_TO_PHOTON_CLAIM_ID, INPUT_TO_PHOTON_SCHEMA_VERSION, INPUT_TO_PHOTON_WORKLOAD_CLASS,
    InputToPhotonInputClass, InputToPhotonState, summarize_input_to_photon_traces,
    unavailable_proxy_evidence,
};
use futures::executor::block_on;
use serde_json::json;
use wgpu::util::DeviceExt;

const GLYPH_DENSE_COLS: usize = 160;
const GLYPH_DENSE_ROWS: usize = 72;
const GPU_FRAME_READBACK_TIMEOUT: Duration = Duration::from_secs(5);
const GPU_FRAME_READBACK_POLL_INTERVAL: Duration = Duration::from_millis(1);
const SHADER_WGSL: &str = include_str!("../../src/shader.wgsl");

const GLYPH_VERTEX_ATTRIBS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x4,
    4 => Float32x3,
    5 => Float32,
    6 => Float32,
];

const GLYPH_QUAD_INSTANCE_POSITION_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![0 => Float32x4];
const GLYPH_QUAD_INSTANCE_TEX_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![1 => Float32x4];
const GLYPH_QUAD_INSTANCE_FG_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![2 => Float32x4];
const GLYPH_QUAD_INSTANCE_ALT_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![3 => Float32x4];
const GLYPH_QUAD_INSTANCE_HSV_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![4 => Float32x3];
const GLYPH_QUAD_INSTANCE_HAS_COLOR_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![5 => Float32];
const GLYPH_QUAD_INSTANCE_MIX_VALUE_ATTRIBS: [wgpu::VertexAttribute; 1] =
    wgpu::vertex_attr_array![6 => Float32];

fn bench_classified_input_to_headless_proxy_frame(c: &mut Criterion) {
    let input = classified_input_headless_fixture();
    if render_headless(&input).is_err() {
        c.bench_function("input_to_photon/headless_unavailable_noop", |b| {
            b.iter(|| black_box(()));
        });
        return;
    }

    c.bench_function("input_to_photon/classified_input_headless_proxy_frame", |b| {
        b.iter(|| {
            let frame = render_headless(black_box(&input))
                .expect("headless renderer must be available for measured Criterion run");
            black_box(frame.rgba.len());
        });
    });
}

fn bench_ft_p4vzl_glyph_dense_gpu_frame_ab(c: &mut Criterion) {
    let fixture = SoaQuadBenchFixture::glyph_dense_frame(GLYPH_DENSE_COLS, GLYPH_DENSE_ROWS);
    let Ok(gpu_bench) = GlyphQuadGpuBench::new(&fixture) else {
        c.bench_function("ft_p4vzl_4/glyph_dense_gpu_frame_unavailable_noop", |b| {
            b.iter(|| black_box(()));
        });
        return;
    };

    c.bench_function(
        "ft_p4vzl_4/glyph_dense_gpu_frame_time__gate_env_FT_MOONSHOT_INSTANCED_GLYPH_QUADS",
        |b| {
            b.iter(|| {
                let use_instanced = moonshot_instanced_glyph_quads_enabled();
                let readback = gpu_bench
                    .render_frame(use_instanced)
                    .expect("glyph-dense GPU frame must render for measured Criterion run");
                black_box(readback);
            });
        },
    );
}

fn bench_ft_3r0yk_soa_quad_staging_toggle(c: &mut Criterion) {
    let fixture = SoaQuadBenchFixture::glyph_dense_frame(GLYPH_DENSE_COLS, GLYPH_DENSE_ROWS);
    c.bench_function("ft_3r0yk/soa_quad_staging_bytes", |b| {
        b.iter(|| {
            let prepared_bytes = if moonshot_instanced_glyph_quads_enabled() {
                fixture.soa_instance_upload_bytes()
            } else {
                fixture.expand_aos_baseline_bytes()
            };
            black_box(prepared_bytes);
        });
    });
}

fn bench_ft_egok5_glyph_run_interning_toggle(c: &mut Criterion) {
    let glyphs = glyph_run_probe_fixture(64);
    c.bench_function("ft_egok5/glyph_run_interning_toggle", |b| {
        b.iter(|| {
            let retained = if glyph_run_interning_enabled() {
                glyph_run_probe_iteration(black_box(&glyphs), 16)
            } else {
                glyph_run_probe_disabled_iteration(black_box(&glyphs), 16)
            };
            black_box(retained);
        });
    });
}

struct SoaQuadBenchFixture {
    instances: Vec<GlyphQuadStagingInstance>,
    positions: Vec<[f32; 4]>,
    tex_rects: Vec<[f32; 4]>,
    fg_colors: Vec<[f32; 4]>,
    alt_colors: Vec<[f32; 4]>,
    hsv: Vec<[f32; 3]>,
    has_color: Vec<f32>,
    mix_values: Vec<f32>,
}

impl SoaQuadBenchFixture {
    fn glyph_dense_frame(cols: usize, rows: usize) -> Self {
        Self::new(cols.max(1), rows.max(1))
    }

    fn new(cols: usize, rows: usize) -> Self {
        let len = cols.saturating_mul(rows).max(1);
        let mut instances = Vec::with_capacity(len);
        let mut positions = Vec::with_capacity(len);
        let mut tex_rects = Vec::with_capacity(len);
        let mut fg_colors = Vec::with_capacity(len);
        let mut alt_colors = Vec::with_capacity(len);
        let mut hsv = Vec::with_capacity(len);
        let mut has_color = Vec::with_capacity(len);
        let mut mix_values = Vec::with_capacity(len);

        let cell_w = 2.0f32 / cols as f32;
        let cell_h = 2.0f32 / rows as f32;
        for idx in 0..len {
            let col = (idx % cols) as f32;
            let row = (idx / cols) as f32;
            let left = -1.0 + col * cell_w;
            let top = -1.0 + row * cell_h;
            let tex_left = ((idx % 32) as f32) / 64.0;
            let tex_top = ((idx / 32) as f32) / 64.0;
            let instance = GlyphQuadStagingInstance::new(
                [left, top, left + cell_w, top + cell_h],
                [tex_left, tex_left + 0.015625, tex_top, tex_top + 0.03125],
                [
                    0.20 + (idx % 5) as f32 * 0.03,
                    0.40 + (idx % 7) as f32 * 0.02,
                    0.72,
                    1.0,
                ],
                [0.88, 0.18 + (idx % 3) as f32 * 0.08, 0.12, 0.70],
                [1.0, 1.0 - (idx % 4) as f32 * 0.05, 0.90],
                idx % 11 == 0,
                (idx % 8) as f32 / 8.0,
            );
            positions.push(instance.position);
            tex_rects.push(instance.tex);
            fg_colors.push(instance.fg_color);
            alt_colors.push(instance.alt_color);
            hsv.push(instance.hsv);
            has_color.push(instance.has_color);
            mix_values.push(instance.mix_value);
            instances.push(instance);
        }

        Self {
            instances,
            positions,
            tex_rects,
            fg_colors,
            alt_colors,
            hsv,
            has_color,
            mix_values,
        }
    }

    fn buffers(&self) -> GlyphQuadSoaBuffers<'_> {
        GlyphQuadSoaBuffers {
            positions: &self.positions,
            tex_rects: &self.tex_rects,
            fg_colors: &self.fg_colors,
            alt_colors: &self.alt_colors,
            hsv: &self.hsv,
            has_color: &self.has_color,
            mix_values: &self.mix_values,
        }
    }

    fn expand_aos_baseline_vertices(&self) -> Vec<GlyphQuadStagingVertex> {
        let mut vertices = Vec::with_capacity(self.instances.len() * VERTICES_PER_GLYPH_QUAD);
        for instance in &self.instances {
            vertices.extend_from_slice(&aos_glyph_quad_vertices(*instance));
        }
        vertices
    }

    fn expand_aos_baseline_bytes(&self) -> usize {
        let vertices = self.expand_aos_baseline_vertices();
        let bytes = vertices.len() * size_of::<GlyphQuadStagingVertex>();
        black_box(vertices);
        bytes
    }

    fn soa_instance_upload_bytes(&self) -> usize {
        let buffers = self.buffers();
        buffers.assert_consistent_lengths();

        let mut checksum = 0.0f32;
        for rect in buffers.positions {
            checksum += rect.iter().copied().sum::<f32>();
        }
        for rect in buffers.tex_rects {
            checksum += rect.iter().copied().sum::<f32>();
        }
        for color in buffers.fg_colors {
            checksum += color.iter().copied().sum::<f32>();
        }
        for color in buffers.alt_colors {
            checksum += color.iter().copied().sum::<f32>();
        }
        for hsv in buffers.hsv {
            checksum += hsv.iter().copied().sum::<f32>();
        }
        for value in buffers.has_color {
            checksum += *value;
        }
        for value in buffers.mix_values {
            checksum += *value;
        }

        black_box(checksum);
        self.positions.len()
            * (size_of::<[f32; 4]>() * 4 + size_of::<[f32; 3]>() + size_of::<f32>() * 2)
    }
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct BenchShaderUniform {
    foreground_text_hsb: [f32; 3],
    milliseconds: u32,
    projection: [[f32; 4]; 4],
}

struct GlyphQuadGpuBench {
    device: wgpu::Device,
    queue: wgpu::Queue,
    render_pipeline: wgpu::RenderPipeline,
    instanced_pipeline: wgpu::RenderPipeline,
    uniform_bind_group: wgpu::BindGroup,
    texture_linear_bind_group: wgpu::BindGroup,
    texture_nearest_bind_group: wgpu::BindGroup,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    aos_vertex_buffer: wgpu::Buffer,
    aos_index_buffer: wgpu::Buffer,
    aos_index_count: u32,
    soa_buffers: GpuSoaBuffers,
}

impl GlyphQuadGpuBench {
    fn new(fixture: &SoaQuadBenchFixture) -> Result<Self, String> {
        block_on(Self::new_async(fixture))
    }

    async fn new_async(fixture: &SoaQuadBenchFixture) -> Result<Self, String> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(descriptor);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|err| err.to_string())?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                label: Some("frankenterm-gui glyph quad frame bench device"),
                memory_hints: Default::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|err| err.to_string())?;

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("frankenterm-gui glyph quad frame bench shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_WGSL.into()),
        });
        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("glyph quad frame bench uniform layout"),
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
            });
        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("glyph quad frame bench texture layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
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
            });
        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("glyph quad frame bench pipeline layout"),
                bind_group_layouts: &[
                    Some(&uniform_bind_group_layout),
                    Some(&texture_bind_group_layout),
                    Some(&texture_bind_group_layout),
                ],
                immediate_size: 0,
            });

        let render_pipeline = create_glyph_quad_render_pipeline(
            &device,
            &render_pipeline_layout,
            &shader,
            format,
            "glyph quad frame bench AoS pipeline",
            "vs_main",
            &[glyph_vertex_buffer_layout()],
            wgpu::PrimitiveTopology::TriangleList,
        );
        let instance_layouts = glyph_quad_instance_buffer_layouts();
        let instanced_pipeline = create_glyph_quad_render_pipeline(
            &device,
            &render_pipeline_layout,
            &shader,
            format,
            "glyph quad frame bench SoA instanced pipeline",
            "vs_instanced_glyph_main",
            &instance_layouts,
            wgpu::PrimitiveTopology::TriangleStrip,
        );

        let uniform = BenchShaderUniform {
            foreground_text_hsb: [1.0, 1.0, 1.0],
            milliseconds: 0,
            projection: identity_projection(),
        };
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph quad frame bench uniform buffer"),
            usage: wgpu::BufferUsages::UNIFORM,
            contents: bytemuck::bytes_of(&uniform),
        });
        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph quad frame bench uniform bind group"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph quad frame bench atlas"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[format],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255; 16],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: Some(2),
            },
            wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let texture_linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph quad frame bench linear sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let texture_nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph quad frame bench nearest sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let texture_linear_bind_group = create_texture_bind_group(
            &device,
            &texture_bind_group_layout,
            &atlas_view,
            &texture_linear_sampler,
            "glyph quad frame bench linear texture bind group",
        );
        let texture_nearest_bind_group = create_texture_bind_group(
            &device,
            &texture_bind_group_layout,
            &atlas_view,
            &texture_nearest_sampler,
            "glyph quad frame bench nearest texture bind group",
        );

        let texture_size = wgpu::Extent3d {
            width: 1280,
            height: 720,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph quad frame bench target"),
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[format],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyph quad frame bench readback"),
            size: u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let aos_vertices = fixture.expand_aos_baseline_vertices();
        let aos_indices = glyph_quad_indices(fixture.instances.len());
        let aos_vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph quad frame bench AoS vertex buffer"),
            usage: wgpu::BufferUsages::VERTEX,
            contents: bytemuck::cast_slice(&aos_vertices),
        });
        let aos_index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("glyph quad frame bench AoS index buffer"),
            usage: wgpu::BufferUsages::INDEX,
            contents: bytemuck::cast_slice(&aos_indices),
        });
        let soa_buffers = GpuSoaBuffers::new(&device, fixture);

        Ok(Self {
            device,
            queue,
            render_pipeline,
            instanced_pipeline,
            uniform_bind_group,
            texture_linear_bind_group,
            texture_nearest_bind_group,
            target,
            target_view,
            readback,
            aos_vertex_buffer,
            aos_index_buffer,
            aos_index_count: u32::try_from(aos_indices.len()).expect("index count fits u32"),
            soa_buffers,
        })
    }

    fn render_frame(&self, use_instanced: bool) -> Result<u8, String> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("glyph quad frame bench encoder"),
            });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("glyph quad frame bench render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
            render_pass.set_bind_group(1, &self.texture_linear_bind_group, &[]);
            render_pass.set_bind_group(2, &self.texture_nearest_bind_group, &[]);
            if use_instanced {
                render_pass.set_pipeline(&self.instanced_pipeline);
                render_pass.set_vertex_buffer(0, self.soa_buffers.positions.slice(..));
                render_pass.set_vertex_buffer(1, self.soa_buffers.tex_rects.slice(..));
                render_pass.set_vertex_buffer(2, self.soa_buffers.fg_colors.slice(..));
                render_pass.set_vertex_buffer(3, self.soa_buffers.alt_colors.slice(..));
                render_pass.set_vertex_buffer(4, self.soa_buffers.hsv.slice(..));
                render_pass.set_vertex_buffer(5, self.soa_buffers.has_color.slice(..));
                render_pass.set_vertex_buffer(6, self.soa_buffers.mix_values.slice(..));
                render_pass.draw(0..4, 0..self.soa_buffers.instance_count);
            } else {
                render_pass.set_pipeline(&self.render_pipeline);
                render_pass.set_vertex_buffer(0, self.aos_vertex_buffer.slice(..));
                render_pass
                    .set_index_buffer(self.aos_index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..self.aos_index_count, 0, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );

        self.queue.submit(std::iter::once(encoder.finish()));
        self.wait_for_readback()
    }

    fn wait_for_readback(&self) -> Result<u8, String> {
        let slice = self
            .readback
            .slice(..u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });

        let started = Instant::now();
        loop {
            self.device
                .poll(wgpu::PollType::Poll)
                .map_err(|err| format!("{err:?}"))?;
            match receiver.recv_timeout(GPU_FRAME_READBACK_POLL_INTERVAL) {
                Ok(Ok(())) => break,
                Ok(Err(err)) => return Err(format!("glyph frame readback failed: {err:?}")),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if started.elapsed() < GPU_FRAME_READBACK_TIMEOUT => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err("glyph frame readback timed out".to_string());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("glyph frame readback callback disconnected".to_string());
                }
            }
        }

        let mapped = slice.get_mapped_range();
        let checksum = mapped.first().copied().unwrap_or_default();
        drop(mapped);
        self.readback.unmap();
        Ok(checksum)
    }
}

struct GpuSoaBuffers {
    positions: wgpu::Buffer,
    tex_rects: wgpu::Buffer,
    fg_colors: wgpu::Buffer,
    alt_colors: wgpu::Buffer,
    hsv: wgpu::Buffer,
    has_color: wgpu::Buffer,
    mix_values: wgpu::Buffer,
    instance_count: u32,
}

impl GpuSoaBuffers {
    fn new(device: &wgpu::Device, fixture: &SoaQuadBenchFixture) -> Self {
        Self {
            positions: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA positions",
                &fixture.positions,
            ),
            tex_rects: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA tex rects",
                &fixture.tex_rects,
            ),
            fg_colors: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA fg colors",
                &fixture.fg_colors,
            ),
            alt_colors: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA alt colors",
                &fixture.alt_colors,
            ),
            hsv: create_vertex_buffer(device, "glyph quad frame bench SoA hsv", &fixture.hsv),
            has_color: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA has color",
                &fixture.has_color,
            ),
            mix_values: create_vertex_buffer(
                device,
                "glyph quad frame bench SoA mix values",
                &fixture.mix_values,
            ),
            instance_count: u32::try_from(fixture.instances.len())
                .expect("instance count fits u32"),
        }
    }
}

fn create_glyph_quad_render_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    label: &'static str,
    vertex_entry_point: &'static str,
    buffers: &[wgpu::VertexBufferLayout<'_>],
    topology: wgpu::PrimitiveTopology,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vertex_entry_point),
            buffers,
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology,
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
        multiview_mask: None,
        cache: None,
    })
}

fn create_texture_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
    label: &'static str,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn create_vertex_buffer<T: bytemuck::Pod>(
    device: &wgpu::Device,
    label: &'static str,
    values: &[T],
) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        usage: wgpu::BufferUsages::VERTEX,
        contents: bytemuck::cast_slice(values),
    })
}

fn glyph_vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<GlyphQuadStagingVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &GLYPH_VERTEX_ATTRIBS,
    }
}

fn glyph_quad_instance_buffer_layouts() -> [wgpu::VertexBufferLayout<'static>; 7] {
    [
        glyph_quad_instance_buffer_layout::<[f32; 4]>(&GLYPH_QUAD_INSTANCE_POSITION_ATTRIBS),
        glyph_quad_instance_buffer_layout::<[f32; 4]>(&GLYPH_QUAD_INSTANCE_TEX_ATTRIBS),
        glyph_quad_instance_buffer_layout::<[f32; 4]>(&GLYPH_QUAD_INSTANCE_FG_ATTRIBS),
        glyph_quad_instance_buffer_layout::<[f32; 4]>(&GLYPH_QUAD_INSTANCE_ALT_ATTRIBS),
        glyph_quad_instance_buffer_layout::<[f32; 3]>(&GLYPH_QUAD_INSTANCE_HSV_ATTRIBS),
        glyph_quad_instance_buffer_layout::<f32>(&GLYPH_QUAD_INSTANCE_HAS_COLOR_ATTRIBS),
        glyph_quad_instance_buffer_layout::<f32>(&GLYPH_QUAD_INSTANCE_MIX_VALUE_ATTRIBS),
    ]
}

fn glyph_quad_instance_buffer_layout<T>(
    attributes: &'static [wgpu::VertexAttribute],
) -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<T>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes,
    }
}

fn glyph_quad_indices(quad_count: usize) -> Vec<u32> {
    let mut indices = Vec::with_capacity(quad_count * 6);
    for quad_idx in 0..quad_count {
        let base = u32::try_from(quad_idx * VERTICES_PER_GLYPH_QUAD).expect("quad index fits u32");
        indices.extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 3, base + 2]);
    }
    indices
}

fn identity_projection() -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

fn glyph_run_probe_fixture(len: u32) -> Vec<GlyphRunProbeGlyph> {
    (0..len)
        .map(|idx| GlyphRunProbeGlyph {
            glyph_pos: 400 + idx,
            cluster: idx,
            font_idx: (idx % 3) as usize,
            x_advance_bits: (8.0f64 + f64::from(idx % 7) / 16.0).to_bits(),
            x_offset_bits: (f64::from(idx % 5) / 32.0).to_bits(),
            glyph_ptr: 0x1000 + idx as usize * 64,
            bitmap_pixel_width: 8 + idx % 11,
            bearing_x_bits: (1.0f64 + f64::from(idx % 9) / 64.0).to_bits(),
        })
        .collect()
}

fn glyph_run_probe_disabled_iteration(glyphs: &[GlyphRunProbeGlyph], repeats: usize) -> usize {
    let mut retained = 0usize;
    for _ in 0..repeats {
        let run = glyphs.to_vec();
        retained = retained
            .wrapping_add(run.len())
            .wrapping_add(run.capacity());
        black_box(run);
    }
    retained
}

fn bench_config() -> Criterion {
    emit_evidence_row();
    Criterion::default().configure_from_args()
}

fn emit_evidence_row() {
    let platform = std::env::consts::OS.to_string();
    let input = classified_input_headless_fixture();
    let evidence = match render_headless(&input) {
        Ok(frame) => {
            let marker_started = Instant::now();
            drop(trace_from_headless_frame(
                0,
                InputToPhotonInputClass::PrintableText,
                1,
                platform.clone(),
                &frame,
                0,
            ));
            let marker_overhead_us = u64::try_from(marker_started.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .max(1);
            let trace = trace_from_headless_frame(
                0,
                InputToPhotonInputClass::PrintableText,
                1,
                platform.clone(),
                &frame,
                marker_overhead_us,
            );
            match trace {
                Ok(trace) => summarize_input_to_photon_traces(platform.clone(), &[trace]),
                Err(reason) => unavailable_proxy_evidence(platform.clone(), reason),
            }
        }
        Err(error) => unavailable_proxy_evidence(platform.clone(), error.to_string()),
    };

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": INPUT_TO_PHOTON_CLAIM_ID,
        "metric_value": evidence.p95_us.map(|value| value as f64 / 1_000.0),
        "metric_unit": "ms",
        "sample_size": evidence.sample_count,
        "commit_sha": option_env!("VERGEN_GIT_SHA"),
        "hardware_fingerprint": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "runner_sku": std::env::var("RUNNER_OS").unwrap_or_else(|_| std::env::consts::OS.to_string()),
        "workload_class": INPUT_TO_PHOTON_WORKLOAD_CLASS,
        "tags": {
            "trace_schema_version": INPUT_TO_PHOTON_SCHEMA_VERSION,
            "claim_scope": evidence.claim_scope.label(),
            "input_class": evidence.input_class.map(InputToPhotonInputClass::label),
            "min_input_byte_count": evidence.min_input_byte_count,
            "max_input_byte_count": evidence.max_input_byte_count,
            "frankenterm_version": env!("CARGO_PKG_VERSION"),
            "renderer_slo_state": state_tag(evidence.state),
            "within_target": evidence.within_target.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string())
        }
    });

    let evidence_path = evidence_path(&platform);
    if let Some(parent) = evidence_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&evidence_path)
    {
        let _ = writeln!(file, "{row}");
    }
    println!(
        "[BENCH] input_to_photon_evidence={}",
        evidence_path.display()
    );
}

fn evidence_path(platform: &str) -> PathBuf {
    let suffix = match platform {
        "macos" => "macos",
        "linux" => "wayland",
        other => other,
    };
    PathBuf::from(format!(
        "target/criterion/slo-input_to_photon_{suffix}.jsonl"
    ))
}

fn state_tag(state: InputToPhotonState) -> &'static str {
    match state {
        InputToPhotonState::Measured => "measured",
        InputToPhotonState::InstrumentationUnavailable => "instrumentation_unavailable",
        InputToPhotonState::PhotonDetectionUnavailable => "photon_detection_unavailable",
        InputToPhotonState::InstrumentationOverheadExceeded => "instrumentation_overhead_exceeded",
        InputToPhotonState::InvalidTrace => "invalid_trace",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_classified_input_to_headless_proxy_frame,
        bench_ft_p4vzl_glyph_dense_gpu_frame_ab,
        bench_ft_3r0yk_soa_quad_staging_toggle,
        bench_ft_egok5_glyph_run_interning_toggle,
);
criterion_main!(benches);
