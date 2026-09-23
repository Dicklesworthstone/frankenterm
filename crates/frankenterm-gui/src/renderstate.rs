use super::glyphcache::GlyphCache;
use super::quad::*;
use super::utilsprites::{RenderMetrics, UtilSprites};
use crate::termwindow::webgpu::{WebGpuState, WebGpuTexture, adapter_info_to_gpu_info};
use ::window::bitmaps::Texture2d;
use ::window::bitmaps::atlas::{AtlasAllocationFailure, OutOfTextureSpace};
use ::window::glium::backend::Context as GliumContext;
use ::window::glium::buffer::{BufferMutSlice, Mapping};
use ::window::glium::{
    CapabilitiesSource, IndexBuffer as GliumIndexBuffer, VertexBuffer as GliumVertexBuffer,
};
use ::window::*;
use anyhow::Context;
use config::ConfigHandle;
use frankenterm_core::{atlas_tier_doctor::TierSwapDoctorReport, atlas_tiered_swap::MemoryBudget};
use frankenterm_font::FontConfiguration;
use frankenterm_gui::glyph_quad_staging::{
    GlyphQuadSoaBuffers, GlyphQuadStagingVertex, visit_expanded_glyph_quad_soa_vertices,
};
use futures::FutureExt;
use std::cell::{Ref, RefCell, RefMut};
use std::convert::TryInto;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wgpu::util::DeviceExt;

const INDICES_PER_CELL: usize = 6;
const QUAD_CAPACITY_GROWTH_GRANULARITY: usize = 128;
const TEXTURE_ATLAS_BYTES_PER_PIXEL: u64 = 4;

fn texture_atlas_footprint_bytes(side: usize) -> u64 {
    (side as u64)
        .saturating_mul(side as u64)
        .saturating_mul(TEXTURE_ATLAS_BYTES_PER_PIXEL)
}

fn max_texture_atlas_side_for_budget(budget_bytes: u64) -> usize {
    let mut side = 1usize;
    while side <= usize::MAX / 2 {
        let next = side * 2;
        if texture_atlas_footprint_bytes(next) > budget_bytes {
            break;
        }
        side = next;
    }
    side
}

fn texture_atlas_vram_budget_bytes() -> u64 {
    MemoryBudget::default().vram_budget_bytes
}

fn enforce_texture_atlas_budget(size: usize) -> Result<(), OutOfTextureSpace> {
    let budget = texture_atlas_vram_budget_bytes();
    if texture_atlas_footprint_bytes(size) <= budget {
        return Ok(());
    }

    Err(OutOfTextureSpace {
        size: None,
        current_size: max_texture_atlas_side_for_budget(budget),
        failure: AtlasAllocationFailure::MemoryBudget,
    })
}

fn build_quad_indices(num_quads: usize) -> Vec<u32> {
    let mut indices = Vec::with_capacity(num_quads * INDICES_PER_CELL);

    for q in 0..num_quads {
        let idx = (q * VERTICES_PER_CELL) as u32;

        // Emit two triangles to form the glyph quad
        indices.extend_from_slice(&[
            idx + V_TOP_LEFT as u32,
            idx + V_TOP_RIGHT as u32,
            idx + V_BOT_LEFT as u32,
            idx + V_TOP_RIGHT as u32,
            idx + V_BOT_RIGHT as u32,
            idx + V_BOT_LEFT as u32,
        ]);
    }

    indices
}

fn round_quad_capacity(need_quads: usize) -> usize {
    if need_quads == 0 {
        return 0;
    }

    need_quads.div_ceil(QUAD_CAPACITY_GROWTH_GRANULARITY) * QUAD_CAPACITY_GROWTH_GRANULARITY
}

#[derive(Clone)]
pub enum RenderContext {
    Glium(Rc<GliumContext>),
    WebGpu(Rc<WebGpuState>),
}

impl RenderContext {
    pub fn allocate_index_buffer(&self, indices: &[u32]) -> anyhow::Result<IndexBuffer> {
        match self {
            Self::Glium(context) => Ok(IndexBuffer::Glium(GliumIndexBuffer::new(
                context,
                glium::index::PrimitiveType::TrianglesList,
                indices,
            )?)),
            Self::WebGpu(state) => Ok(IndexBuffer::WebGpu(WebGpuIndexBuffer::new(indices, state))),
        }
    }

    pub fn allocate_vertex_buffer_initializer(&self, num_quads: usize) -> Vec<Vertex> {
        match self {
            Self::Glium(_) => {
                vec![Vertex::default(); num_quads * VERTICES_PER_CELL]
            }
            Self::WebGpu(_) => vec![],
        }
    }

    pub fn allocate_vertex_buffer(
        &self,
        num_quads: usize,
        initializer: &[Vertex],
    ) -> anyhow::Result<VertexBuffer> {
        match self {
            Self::Glium(context) => Ok(VertexBuffer::Glium(GliumVertexBuffer::dynamic(
                context,
                initializer,
            )?)),
            Self::WebGpu(state) => Ok(VertexBuffer::WebGpu(WebGpuVertexBuffer::new(
                num_quads * VERTICES_PER_CELL,
                &state.device,
                &state.queue,
            ))),
        }
    }

    pub fn allocate_texture_atlas(&self, size: usize) -> anyhow::Result<Rc<dyn Texture2d>> {
        enforce_texture_atlas_budget(size)?;
        match self {
            Self::Glium(context) => {
                let caps = context.get_capabilities();
                // You'd hope that allocating a texture would automatically
                // include this check, but it doesn't, and instead, the texture
                // silently fails to bind when attempting to render into it later.
                // So! We check and raise here for ourselves!
                let max_texture_size: usize = caps
                    .max_texture_size
                    .try_into()
                    .context("represent Capabilities.max_texture_size as usize")?;
                if size > max_texture_size {
                    anyhow::bail!(
                        "Cannot use a texture of size {} as it is larger \
                         than the max {} supported by your GPU",
                        size,
                        caps.max_texture_size
                    );
                }
                use crate::glium::texture::SrgbTexture2d;
                let surface: Rc<dyn Texture2d> = Rc::new(SrgbTexture2d::empty_with_format(
                    context,
                    glium::texture::SrgbFormat::U8U8U8U8,
                    glium::texture::MipmapsOption::NoMipmap,
                    size as u32,
                    size as u32,
                )?);
                Ok(surface)
            }
            Self::WebGpu(state) => {
                let texture: Rc<dyn Texture2d> =
                    Rc::new(WebGpuTexture::new(size as u32, size as u32, state)?);
                Ok(texture)
            }
        }
    }

    pub fn renderer_info(&self) -> String {
        match self {
            Self::Glium(ctx) => format!(
                "OpenGL: {} {}",
                ctx.get_opengl_renderer_string(),
                ctx.get_opengl_version_string()
            ),
            Self::WebGpu(state) => {
                let info = adapter_info_to_gpu_info(state.adapter_info.clone());
                format!("WebGPU: {}", info.to_string())
            }
        }
    }
}

pub enum IndexBuffer {
    Glium(GliumIndexBuffer<u32>),
    WebGpu(WebGpuIndexBuffer),
}

impl IndexBuffer {
    pub fn glium(&self) -> &GliumIndexBuffer<u32> {
        match self {
            Self::Glium(g) => g,
            _ => unreachable!(),
        }
    }
    pub fn webgpu(&self) -> &WebGpuIndexBuffer {
        match self {
            Self::WebGpu(g) => g,
            _ => unreachable!(),
        }
    }
}

pub enum VertexBuffer {
    Glium(GliumVertexBuffer<Vertex>),
    WebGpu(WebGpuVertexBuffer),
}

impl VertexBuffer {
    pub fn glium(&self) -> &GliumVertexBuffer<Vertex> {
        match self {
            Self::Glium(g) => g,
            _ => unreachable!(),
        }
    }
    pub fn webgpu(&self) -> &WebGpuVertexBuffer {
        match self {
            Self::WebGpu(g) => g,
            _ => unreachable!(),
        }
    }
    pub fn webgpu_mut(&mut self) -> &mut WebGpuVertexBuffer {
        match self {
            Self::WebGpu(g) => g,
            _ => unreachable!(),
        }
    }
}

enum MappedVertexBuffer {
    Glium(GliumMappedVertexBuffer),
    WebGpu(RefMut<'static, VertexBuffer>),
}

impl MappedVertexBuffer {
    fn slice_mut(&mut self, range: std::ops::Range<usize>) -> &mut [Vertex] {
        match self {
            Self::Glium(g) => &mut g.mapping[range],
            Self::WebGpu(g) => {
                let buffer = g.webgpu_mut();
                assert!(range.end <= buffer.num_vertices);
                // New quads have the same zero initialization as the former
                // mapped-at-creation buffer. Initialize only the used prefix,
                // retaining earlier quads across allocator borrows this frame.
                if range.end > buffer.staging.len() {
                    buffer.staging.resize(range.end, Vertex::default());
                }
                &mut buffer.staging[range]
            }
        }
    }
}

pub struct MappedQuads<'a> {
    mapping: MappedVertexBuffer,
    next: RefMut<'a, usize>,
    capacity: usize,
    glyph_quad_instances: RefMut<'a, WebGpuGlyphQuadSoaStaging>,
}

#[derive(Debug, Default)]
pub struct WebGpuGlyphQuadSoaStaging {
    positions: Vec<[f32; 4]>,
    tex_rects: Vec<[f32; 4]>,
    fg_colors: Vec<[f32; 4]>,
    alt_colors: Vec<[f32; 4]>,
    hsv: Vec<[f32; 3]>,
    has_color: Vec<f32>,
    mix_values: Vec<f32>,
}

impl WebGpuGlyphQuadSoaStaging {
    fn clear(&mut self) {
        self.positions.clear();
        self.tex_rects.clear();
        self.fg_colors.clear();
        self.alt_colors.clear();
        self.hsv.clear();
        self.has_color.clear();
        self.mix_values.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn buffers(&self) -> GlyphQuadSoaBuffers<'_> {
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

    fn extend_from_buffers(&mut self, buffers: GlyphQuadSoaBuffers<'_>) {
        buffers.assert_consistent_lengths();
        self.positions.extend_from_slice(buffers.positions);
        self.tex_rects.extend_from_slice(buffers.tex_rects);
        self.fg_colors.extend_from_slice(buffers.fg_colors);
        self.alt_colors.extend_from_slice(buffers.alt_colors);
        self.hsv.extend_from_slice(buffers.hsv);
        self.has_color.extend_from_slice(buffers.has_color);
        self.mix_values.extend_from_slice(buffers.mix_values);
    }
}

pub struct WebGpuVertexBuffer {
    buf: wgpu::Buffer,
    num_vertices: usize,
    staging: Vec<Vertex>,
    queue: wgpu::Queue,
}

impl std::ops::Deref for WebGpuVertexBuffer {
    type Target = wgpu::Buffer;
    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl WebGpuVertexBuffer {
    pub fn new(num_vertices: usize, device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        metrics::counter!("gui.webgpu.vertex_buffer_allocations").increment(1);
        Self {
            buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Vertex Buffer"),
                size: (num_vertices * std::mem::size_of::<Vertex>()) as wgpu::BufferAddress,
                usage: wgpu::BufferUsages::VERTEX
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
            num_vertices,
            staging: Vec::with_capacity(num_vertices),
            queue: queue.clone(),
        }
    }

    pub fn upload(&self, vertex_count: usize) -> wgpu::Buffer {
        let bytes = bytemuck::cast_slice(&self.staging[..vertex_count]);
        if !bytes.is_empty() {
            // Queue writes copy CPU bytes immediately and execute before the
            // next submitted draw, after prior submissions using this buffer.
            // No synchronous GPU wait or full-capacity buffer recreation.
            self.queue.write_buffer(&self.buf, 0, bytes);
            metrics::histogram!("gui.webgpu.vertex_upload_bytes").record(bytes.len() as f64);
        }
        self.buf.clone()
    }
}

pub struct WebGpuIndexBuffer {
    buf: wgpu::Buffer,
}

impl std::ops::Deref for WebGpuIndexBuffer {
    type Target = wgpu::Buffer;
    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl WebGpuIndexBuffer {
    pub fn new(indices: &[u32], state: &WebGpuState) -> Self {
        Self::with_device(indices, &state.device)
    }

    fn with_device(indices: &[u32], device: &wgpu::Device) -> Self {
        Self {
            buf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Index Buffer"),
                usage: wgpu::BufferUsages::INDEX,
                contents: bytemuck::cast_slice(indices),
            }),
        }
    }
}

/// This is a self-referential struct, but since those are not possible
/// to create safely in unstable rust, we transmute the lifetimes away
/// to static and store the owner (RefMut) and the derived Mapping object
/// in this struct
pub struct GliumMappedVertexBuffer {
    mapping: Mapping<'static, [Vertex]>,
    // Drop the owner after the mapping
    _owner: RefMut<'static, VertexBuffer>,
}

impl<'a> QuadAllocator for MappedQuads<'a> {
    fn allocate<'b>(&'b mut self) -> anyhow::Result<QuadImpl<'b>> {
        let idx = *self.next;
        *self.next += 1;
        let idx = if idx >= self.capacity {
            // We don't have enough quads, so we'll keep re-using
            // the first quad until we reach the end of the render
            // pass, at which point we'll detect this condition
            // and re-allocate the quads.
            0
        } else {
            idx
        };

        let idx = idx * VERTICES_PER_CELL;
        let mut quad = Quad {
            vert: self.mapping.slice_mut(idx..idx + VERTICES_PER_CELL),
        };

        quad.set_has_color(false);

        Ok(QuadImpl::Vert(quad))
    }

    fn extend_with(&mut self, vertices: &[Vertex]) {
        let idx = *self.next;
        let len = vertices.len();

        // idx and next are number of quads, so divide by number of vertices
        *self.next += len / VERTICES_PER_CELL;
        // Only copy in if there is enough room.
        // We'll detect the out of space condition at the end of
        // the render pass.
        let idx = idx * VERTICES_PER_CELL;
        let capacity = self.capacity * VERTICES_PER_CELL;
        if idx + len <= capacity {
            self.mapping
                .slice_mut(idx..idx + len)
                .copy_from_slice(vertices);
        }
    }
}

impl MappedQuads<'_> {
    pub fn extend_with_glyph_quad_soa(&mut self, buffers: GlyphQuadSoaBuffers<'_>) {
        if buffers.is_empty() {
            return;
        }

        if matches!(self.mapping, MappedVertexBuffer::WebGpu(_)) && *self.next == 0 {
            self.glyph_quad_instances.extend_from_buffers(buffers);
            return;
        }

        let mut vertices = Vec::with_capacity(buffers.len() * VERTICES_PER_CELL);
        visit_expanded_glyph_quad_soa_vertices(buffers, |vertex| {
            vertices.push(vertex_from_glyph_quad_staging(vertex));
        });
        self.extend_with(&vertices);
    }
}

fn vertex_from_glyph_quad_staging(vertex: GlyphQuadStagingVertex) -> Vertex {
    Vertex {
        position: vertex.position,
        tex: vertex.tex,
        fg_color: vertex.fg_color,
        alt_color: vertex.alt_color,
        hsv: vertex.hsv,
        has_color: vertex.has_color,
        mix_value: vertex.mix_value,
    }
}

pub struct TripleVertexBuffer {
    pub index: RefCell<usize>,
    pub bufs: RefCell<[VertexBuffer; 3]>,
    glyph_quad_instances: RefCell<[WebGpuGlyphQuadSoaStaging; 3]>,
    pub indices: IndexBuffer,
    pub capacity: usize,
    pub next_quad: RefCell<usize>,
}

/// A trait to avoid broadly-scoped transmutes; we only want to
/// transmute to extend a lifetime to static, and not to change
/// the underlying type.
/// These ExtendStatic trait impls constrain the transmutes in that way,
/// so that the type checker can still catch issues.
unsafe trait ExtendStatic {
    type T;
    unsafe fn extend_lifetime(self) -> Self::T;
}

unsafe impl<'a, T: 'static> ExtendStatic for Ref<'a, T> {
    type T = Ref<'static, T>;
    unsafe fn extend_lifetime(self) -> Self::T {
        unsafe { std::mem::transmute(self) }
    }
}

unsafe impl<'a, T: 'static> ExtendStatic for RefMut<'a, T> {
    type T = RefMut<'static, T>;
    unsafe fn extend_lifetime(self) -> Self::T {
        unsafe { std::mem::transmute(self) }
    }
}

unsafe impl<'a> ExtendStatic for MappedQuads<'a> {
    type T = MappedQuads<'static>;
    unsafe fn extend_lifetime(self) -> Self::T {
        unsafe { std::mem::transmute(self) }
    }
}

unsafe impl<'a, T: ?Sized + ::window::glium::buffer::Content + 'static> ExtendStatic
    for BufferMutSlice<'a, T>
{
    type T = BufferMutSlice<'static, T>;
    unsafe fn extend_lifetime(self) -> Self::T {
        unsafe { std::mem::transmute(self) }
    }
}

impl TripleVertexBuffer {
    fn new_webgpu(num_quads: usize, device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self {
            index: RefCell::new(0),
            bufs: RefCell::new(std::array::from_fn(|_| {
                VertexBuffer::WebGpu(WebGpuVertexBuffer::new(
                    num_quads * VERTICES_PER_CELL,
                    device,
                    queue,
                ))
            })),
            glyph_quad_instances: RefCell::new(std::array::from_fn(|_| {
                WebGpuGlyphQuadSoaStaging::default()
            })),
            indices: IndexBuffer::WebGpu(WebGpuIndexBuffer::with_device(
                &build_quad_indices(num_quads),
                device,
            )),
            capacity: num_quads,
            next_quad: RefCell::new(0),
        }
    }

    pub fn clear_quad_allocation(&self) {
        *self.next_quad.borrow_mut() = 0;
        for buffer in self.bufs.borrow_mut().iter_mut() {
            if let VertexBuffer::WebGpu(buffer) = buffer {
                buffer.staging.clear();
            }
        }
        for instances in self.glyph_quad_instances.borrow_mut().iter_mut() {
            instances.clear();
        }
    }

    pub fn need_more_quads(&self) -> Option<usize> {
        let next = *self.next_quad.borrow();
        if next > self.capacity {
            Some(next)
        } else {
            None
        }
    }

    pub fn vertex_index_count(&self) -> (usize, usize) {
        let num_quads = *self.next_quad.borrow();
        (num_quads * VERTICES_PER_CELL, num_quads * INDICES_PER_CELL)
    }

    pub fn map(&self) -> MappedQuads<'_> {
        let index = *self.index.borrow();
        let glyph_quad_instances = unsafe {
            RefMut::map(self.glyph_quad_instances.borrow_mut(), |instances| {
                &mut instances[index]
            })
            .extend_lifetime()
        };
        let mut bufs = self.current_vb_mut();

        // To map the vertex buffer, we need to hold a mutable reference to
        // the buffer and hold the mapping object alive for the duration
        // of the access.  Rust doesn't allow us to create a struct that
        // holds both of those things, because one references the other
        // and it doesn't permit self-referential structs.
        // We use the very blunt instrument "transmute" to force Rust to
        // treat the lifetimes of both of these things as static, which
        // we can then store in the same struct.
        // This is "safe" because we carry them around together and ensure
        // that the owner is dropped after the derived data.
        let mapping = match &mut *bufs {
            VertexBuffer::Glium(vb) => {
                let buf_slice = unsafe {
                    vb.slice_mut(..)
                        .expect("to map vertex buffer")
                        .extend_lifetime()
                };
                let mapping = buf_slice.map();

                MappedVertexBuffer::Glium(GliumMappedVertexBuffer {
                    _owner: bufs,
                    mapping,
                })
            }
            VertexBuffer::WebGpu(_) => MappedVertexBuffer::WebGpu(bufs),
        };

        MappedQuads {
            mapping,
            next: self.next_quad.borrow_mut(),
            capacity: self.capacity,
            glyph_quad_instances,
        }
    }

    pub fn current_vb_mut(&self) -> RefMut<'static, VertexBuffer> {
        let index = *self.index.borrow();
        let bufs = self.bufs.borrow_mut();
        unsafe { RefMut::map(bufs, |bufs| &mut bufs[index]).extend_lifetime() }
    }

    pub fn current_glyph_quad_instances(&self) -> Ref<'_, WebGpuGlyphQuadSoaStaging> {
        let index = *self.index.borrow();
        Ref::map(self.glyph_quad_instances.borrow(), |instances| {
            &instances[index]
        })
    }

    pub fn next_index(&self) {
        let mut index = self.index.borrow_mut();
        *index += 1;
        if *index >= 3 {
            *index = 0;
        }
    }
}

pub struct RenderLayer {
    pub vb: Rc<RefCell<[TripleVertexBuffer; 3]>>,
    shrink_usage: RefCell<[Option<QuadShrinkUsage>; 3]>,
    context: RenderContext,
    zindex: i8,
}

impl RenderLayer {
    pub fn new(context: &RenderContext, num_quads: usize, zindex: i8) -> anyhow::Result<Self> {
        let vb = [
            Self::compute_vertices(context, 32)?,
            Self::compute_vertices(context, num_quads)?,
            Self::compute_vertices(context, 32)?,
        ];

        Ok(Self {
            context: context.clone(),
            vb: Rc::new(RefCell::new(vb)),
            shrink_usage: RefCell::new([None; 3]),
            zindex,
        })
    }

    pub fn clear_quad_allocation(&self) {
        for vb in self.vb.borrow().iter() {
            vb.clear_quad_allocation();
        }
    }

    pub fn quad_allocator(&self) -> TripleLayerQuadAllocator<'_> {
        // We're creating a self-referential struct here to manage the lifetimes
        // of these related items.  The transmutes are safe because we're only
        // transmuting the lifetimes (not the types), and we're keeping hold
        // of the owner in the returned struct.
        unsafe {
            let vbs = self.vb.borrow().extend_lifetime();
            let layer0 = vbs[0].map().extend_lifetime();
            let layer1 = vbs[1].map().extend_lifetime();
            let layer2 = vbs[2].map().extend_lifetime();
            TripleLayerQuadAllocator::Gpu(BorrowedLayers {
                layers: [layer0, layer1, layer2],
                _owner: vbs,
            })
        }
    }

    pub fn need_more_quads(&self, vb_idx: usize) -> Option<usize> {
        self.vb.borrow()[vb_idx].need_more_quads()
    }

    pub fn reallocate_quads(&self, idx: usize, num_quads: usize) -> anyhow::Result<()> {
        let vb = Self::compute_vertices(&self.context, num_quads)?;
        self.vb.borrow_mut()[idx] = vb;
        Ok(())
    }

    /// Compute a vertex buffer to hold the quads that comprise the visible
    /// portion of the screen.   We recreate this when the screen is resized.
    /// The idea is that we want to minimize any heavy lifting and computation
    /// and instead just poke some attributes into the offset that corresponds
    /// to a changed cell when we need to repaint the screen, and then just
    /// let the GPU figure out the rest.
    fn compute_vertices(
        context: &RenderContext,
        num_quads: usize,
    ) -> anyhow::Result<TripleVertexBuffer> {
        if let RenderContext::WebGpu(state) = context {
            return Ok(TripleVertexBuffer::new_webgpu(
                num_quads,
                &state.device,
                &state.queue,
            ));
        }
        let verts = context.allocate_vertex_buffer_initializer(num_quads);
        log::trace!(
            "compute_vertices num_quads={}, allocated {} bytes",
            num_quads,
            verts.len() * std::mem::size_of::<Vertex>()
        );
        let indices = build_quad_indices(num_quads);

        let buffer = TripleVertexBuffer {
            index: RefCell::new(0),
            bufs: RefCell::new([
                context.allocate_vertex_buffer(num_quads, &verts)?,
                context.allocate_vertex_buffer(num_quads, &verts)?,
                context.allocate_vertex_buffer(num_quads, &verts)?,
            ]),
            glyph_quad_instances: RefCell::new(std::array::from_fn(|_| {
                WebGpuGlyphQuadSoaStaging::default()
            })),
            capacity: num_quads,
            indices: context.allocate_index_buffer(&indices)?,
            next_quad: RefCell::new(0),
        };

        Ok(buffer)
    }
}

pub struct BorrowedLayers {
    pub layers: [MappedQuads<'static>; 3],

    // layers references _owner, so it must be dropped after layers.
    _owner: Ref<'static, [TripleVertexBuffer; 3]>,
}

impl TripleLayerQuadAllocatorTrait for BorrowedLayers {
    fn allocate(&mut self, layer_num: usize) -> anyhow::Result<QuadImpl<'_>> {
        self.layers[layer_num].allocate()
    }

    fn extend_with(&mut self, layer_num: usize, vertices: &[Vertex]) {
        self.layers[layer_num].extend_with(vertices)
    }

    fn extend_with_glyph_quad_soa(&mut self, layer_num: usize, buffers: GlyphQuadSoaBuffers<'_>) {
        self.layers[layer_num].extend_with_glyph_quad_soa(buffers)
    }
}

pub struct RenderState {
    pub context: RenderContext,
    pub glyph_cache: RefCell<GlyphCache>,
    pub util_sprites: UtilSprites,
    pub glyph_prog: Option<glium::Program>,
    pub layers: RefCell<Vec<Rc<RenderLayer>>>,
    quad_last_activity: Instant,
    pending_quad_shrink: Option<PendingQuadShrink>,
}

const QUAD_SHRINK_IDLE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct QuadShrinkUsage {
    target: usize,
    since: Instant,
}

impl QuadShrinkUsage {
    fn observe(previous: Option<Self>, used: usize, now: Instant, submitted: bool) -> Option<Self> {
        if !submitted {
            return None;
        }
        let target = quad_shrink_target(used);
        Some(match previous {
            Some(prior) if prior.target == target => prior,
            _ => Self { target, since: now },
        })
    }

    fn eligible(self, capacity: usize, now: Instant) -> bool {
        now.saturating_duration_since(self.since) >= QUAD_SHRINK_IDLE && self.target <= capacity / 2
    }
}

fn quad_shrink_target(used: usize) -> usize {
    used.checked_mul(2)
        .map(|size| size.max(128).div_ceil(QUAD_CAPACITY_GROWTH_GRANULARITY))
        .and_then(|buckets| buckets.checked_mul(QUAD_CAPACITY_GROWTH_GRANULARITY))
        .unwrap_or(usize::MAX)
}

struct PreparedQuadShrink {
    owner: Rc<RefCell<[TripleVertexBuffer; 3]>>,
    index: usize,
    previous_capacity: usize,
    replacement: TripleVertexBuffer,
}

struct PendingQuadShrink {
    activity: Instant,
    started: Instant,
    prepared: Option<anyhow::Result<Vec<PreparedQuadShrink>>>,
    ready: futures::future::LocalBoxFuture<'static, anyhow::Result<()>>,
}

fn finish_quad_shrink_allocation(
    scopes: Vec<wgpu::ErrorScopeGuard>,
) -> futures::future::LocalBoxFuture<'static, anyhow::Result<()>> {
    // Pop synchronously in stack order; completion never escapes its scope.
    let errors: Vec<_> = scopes
        .into_iter()
        .rev()
        .map(|scope| scope.pop().boxed_local())
        .collect();
    async move {
        let errors = futures::future::join_all(errors).await;
        if let Some(error) = errors.into_iter().flatten().next() {
            anyhow::bail!("quad shrink allocation rejected: {error}");
        }
        Ok(())
    }
    .boxed_local()
}

impl PendingQuadShrink {
    fn cancel_if_stale(&mut self, now: Instant, activity: Instant, resizing: bool) {
        if resizing
            || activity != self.activity
            || now.saturating_duration_since(self.started) > Duration::from_secs(5)
        {
            // Drop all replacement resources now, retaining only the single
            // scope completion future until it resolves (or its window dies).
            self.prepared.take();
        }
    }
}

/// Publish only after every allocation and backend error scope has succeeded.
/// No borrowed/mapped frame state may exist at this frame-boundary seam.
fn commit_quad_shrink(prepared: Vec<PreparedQuadShrink>) -> anyhow::Result<u64> {
    for item in &prepared {
        let owner = item.owner.try_borrow()?;
        anyhow::ensure!(
            owner[item.index].capacity == item.previous_capacity,
            "quad capacity changed during shrink preparation"
        );
        anyhow::ensure!(
            item.replacement.capacity < item.previous_capacity,
            "quad shrink did not reduce allocation"
        );
    }
    // Preflight every mutable borrow before the first mutation.
    for item in &prepared {
        drop(item.owner.try_borrow_mut()?);
    }
    let count = prepared.len() as u64;
    for item in prepared {
        item.owner.borrow_mut()[item.index] = item.replacement;
    }
    Ok(count)
}

/// Live aggregate of the quad vertex-buffer allocation owned by `RenderState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuadAllocationSnapshot {
    pub capacity: usize,
    pub used: usize,
}

/// Result of a quad-buffer allocation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuadAllocationChange {
    pub allocated: bool,
    pub reallocation_count: u64,
}

impl RenderState {
    pub(crate) fn note_quad_activity(&mut self, now: Instant) {
        self.quad_last_activity = now;
        if let Some(pending) = self.pending_quad_shrink.as_mut() {
            pending.prepared.take();
        }
    }

    /// Failed/partial paint passes are never a usage baseline for reclamation.
    pub(crate) fn observe_quad_frame(&mut self, now: Instant, submitted: bool) {
        for layer in self.layers.borrow().iter() {
            let buffers = layer.vb.borrow();
            let mut usage = layer.shrink_usage.borrow_mut();
            for (index, buffer) in buffers.iter().enumerate() {
                usage[index] = QuadShrinkUsage::observe(
                    usage[index],
                    *buffer.next_quad.borrow(),
                    now,
                    submitted,
                );
            }
        }
    }

    fn quad_shrink_candidates(&self, now: Instant) -> Vec<(Rc<RenderLayer>, usize, usize)> {
        if now.saturating_duration_since(self.quad_last_activity) < QUAD_SHRINK_IDLE {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for layer in self.layers.borrow().iter() {
            let buffers = layer.vb.borrow();
            for (index, observed) in layer.shrink_usage.borrow().iter().enumerate() {
                if let Some(observed) = observed {
                    // Headroom plus 2:1 hysteresis prevents resizing on a
                    // cursor blink or small alternating geometry changes.
                    if observed.eligible(buffers[index].capacity, now) {
                        candidates.push((Rc::clone(layer), index, observed.target));
                    }
                }
            }
        }
        candidates
    }

    pub(crate) fn quad_shrink_needs_paint(&mut self, now: Instant, resizing: bool) -> bool {
        if let Some(pending) = self.pending_quad_shrink.as_mut() {
            pending.cancel_if_stale(now, self.quad_last_activity, resizing);
            if pending.prepared.is_none() {
                if let Some(result) = pending.ready.as_mut().now_or_never() {
                    if let Err(error) = result {
                        log::warn!("cancelled quad shrink allocation rejected: {error:#}");
                    }
                    self.pending_quad_shrink.take();
                    self.quad_last_activity = now;
                }
                return false;
            }
        }
        !resizing
            && (self.pending_quad_shrink.is_some() || !self.quad_shrink_candidates(now).is_empty())
    }

    /// Run before paint acquires any mapped/borrowed layer storage. All new
    /// resources are checked before any old resources are replaced. Queued GPU
    /// commands retain their own references to the previous buffers.
    pub(crate) fn shrink_idle_quads(
        &mut self,
        now: Instant,
        resizing: bool,
    ) -> anyhow::Result<u64> {
        if resizing {
            self.note_quad_activity(now);
            return Ok(0);
        }
        if self.pending_quad_shrink.is_none() {
            let candidates = self.quad_shrink_candidates(now);
            if candidates.is_empty() {
                return Ok(0);
            }
            let scopes = match &self.context {
                RenderContext::WebGpu(state) => vec![
                    state
                        .device
                        .push_error_scope(wgpu::ErrorFilter::OutOfMemory),
                    state.device.push_error_scope(wgpu::ErrorFilter::Internal),
                    state.device.push_error_scope(wgpu::ErrorFilter::Validation),
                ],
                RenderContext::Glium(_) => Vec::new(),
            };
            let prepared = candidates
                .into_iter()
                .map(|(layer, index, target)| {
                    let previous_capacity = layer.vb.borrow()[index].capacity;
                    Ok(PreparedQuadShrink {
                        replacement: RenderLayer::compute_vertices(&layer.context, target)?,
                        owner: Rc::clone(&layer.vb),
                        index,
                        previous_capacity,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>();
            // Pop synchronously in stack order. Retain every completion future
            // even if another scope reports an error; never await on the GUI.
            self.pending_quad_shrink = Some(PendingQuadShrink {
                activity: self.quad_last_activity,
                started: now,
                prepared: Some(prepared),
                ready: finish_quad_shrink_allocation(scopes),
            });
        }
        let pending = self.pending_quad_shrink.as_mut().unwrap();
        pending.cancel_if_stale(now, self.quad_last_activity, resizing);
        let Some(result) = pending.ready.as_mut().now_or_never() else {
            // One retained batch, polled by the existing periodic status tick.
            // No repeated allocations or synchronous GPU completion wait.
            return Ok(0);
        };
        let mut pending = self.pending_quad_shrink.take().unwrap();
        // Back off after failure/cancellation as well as successful shrink.
        let candidates = self.quad_shrink_candidates(now);
        let current = pending.activity == self.quad_last_activity
            && now.saturating_duration_since(pending.started) <= Duration::from_secs(5);
        self.quad_last_activity = now;
        result?;
        let Some(prepared) = pending.prepared.take() else {
            return Ok(0);
        };
        let prepared = prepared?;
        if !current
            || prepared.iter().any(|item| {
                !candidates.iter().any(|(layer, index, target)| {
                    Rc::ptr_eq(&layer.vb, &item.owner)
                        && *index == item.index
                        && *target == item.replacement.capacity
                })
            })
        {
            return Ok(0);
        }
        let capacity_before = self.quad_allocation_snapshot().capacity;
        let count = commit_quad_shrink(prepared)?;
        let capacity_after = self.quad_allocation_snapshot().capacity;
        log::debug!(
            "idle quad allocation reduced: capacity_quads={capacity_before}->{capacity_after} replaced_quad_sets={count}; queued GPU references may still be live"
        );
        Ok(count)
    }

    pub fn new(
        context: RenderContext,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        mut atlas_size: usize,
    ) -> anyhow::Result<Self> {
        loop {
            let glyph_cache = RefCell::new(GlyphCache::new_gl(&context, fonts, atlas_size)?);
            let result = UtilSprites::new(&mut *glyph_cache.borrow_mut(), metrics);
            match result {
                Ok(util_sprites) => {
                    let glyph_prog = match &context {
                        RenderContext::Glium(context) => {
                            Some(Self::compile_prog(&context, Self::glyph_shader)?)
                        }
                        RenderContext::WebGpu(_) => None,
                    };

                    let main_layer = Rc::new(RenderLayer::new(&context, 1024, 0)?);

                    return Ok(Self {
                        context,
                        glyph_cache,
                        util_sprites,
                        glyph_prog,
                        layers: RefCell::new(vec![main_layer]),
                        quad_last_activity: Instant::now(),
                        pending_quad_shrink: None,
                    });
                }
                Err(OutOfTextureSpace {
                    size: Some(size),
                    failure: AtlasAllocationFailure::Capacity,
                    ..
                }) => {
                    atlas_size = size;
                }
                Err(err) => return Err(err.into()),
            };
        }
    }

    pub fn layer_for_zindex(&self, zindex: i8) -> anyhow::Result<Rc<RenderLayer>> {
        if let Some(layer) = self
            .layers
            .borrow()
            .iter()
            .find(|l| l.zindex == zindex)
            .map(Rc::clone)
        {
            return Ok(layer);
        }

        let layer = Rc::new(RenderLayer::new(&self.context, 128, zindex)?);
        let mut layers = self.layers.borrow_mut();
        layers.push(Rc::clone(&layer));

        // Keep the layers sorted by zindex so that they are rendered in
        // the correct order when the layers array is iterated.
        layers.sort_by(|a, b| a.zindex.cmp(&b.zindex));

        Ok(layer)
    }

    /// Returns true if any of the layers needed more quads to be allocated,
    /// and if we successfully allocated them.
    /// Returns false if the quads were sufficient.
    /// Returns Err if we needed to allocate but failed.
    pub fn allocated_more_quads(&mut self) -> anyhow::Result<bool> {
        Ok(self.allocate_more_quads()?.allocated)
    }

    /// Allocate any undersized quad buffers and report how many
    /// concrete GPU-buffer reallocations were performed.
    pub fn allocate_more_quads(&mut self) -> anyhow::Result<QuadAllocationChange> {
        let mut result = QuadAllocationChange::default();

        for layer in self.layers.borrow().iter() {
            for vb_idx in 0..3 {
                if let Some(need_quads) = layer.need_more_quads(vb_idx) {
                    // Round up to the next allocation bucket so bursty frames
                    // don't trigger repeated tiny reallocations.
                    let num_quads = round_quad_capacity(need_quads);
                    layer.reallocate_quads(vb_idx, num_quads).with_context(|| {
                        format!(
                            "Failed to allocate {} quads (needed {})",
                            num_quads, need_quads,
                        )
                    })?;
                    log::trace!("Allocated {} quads (needed {})", num_quads, need_quads);
                    result.allocated = true;
                    result.reallocation_count = result.reallocation_count.saturating_add(1);
                }
            }
        }

        Ok(result)
    }

    pub fn needs_more_quads(&self) -> bool {
        self.layers
            .borrow()
            .iter()
            .any(|layer| (0..3).any(|vb_idx| layer.need_more_quads(vb_idx).is_some()))
    }

    pub fn quad_allocation_snapshot(&self) -> QuadAllocationSnapshot {
        let mut snapshot = QuadAllocationSnapshot::default();
        for layer in self.layers.borrow().iter() {
            for buffer in layer.vb.borrow().iter() {
                snapshot.capacity = snapshot.capacity.saturating_add(buffer.capacity);
                snapshot.used = snapshot.used.saturating_add(*buffer.next_quad.borrow());
            }
        }
        snapshot
    }

    fn compile_prog(
        context: &Rc<GliumContext>,
        fragment_shader: fn(&str) -> (String, String),
    ) -> anyhow::Result<glium::Program> {
        let mut errors = vec![];

        let caps = context.get_capabilities();
        log::trace!("Compiling shader. context.capabilities.srgb={}", caps.srgb);

        for version in &["330 core", "330", "320 es", "300 es"] {
            let (vertex_shader, fragment_shader) = fragment_shader(version);
            let source = glium::program::ProgramCreationInput::SourceCode {
                vertex_shader: &vertex_shader,
                fragment_shader: &fragment_shader,
                outputs_srgb: true,
                tessellation_control_shader: None,
                tessellation_evaluation_shader: None,
                transform_feedback_varyings: None,
                uses_point_size: false,
                geometry_shader: None,
            };
            match glium::Program::new(context, source) {
                Ok(prog) => {
                    return Ok(prog);
                }
                Err(err) => errors.push(format!("shader version: {}: {:#}", version, err)),
            };
        }

        anyhow::bail!("Failed to compile shaders: {}", errors.join("\n"))
    }

    fn glyph_shader(version: &str) -> (String, String) {
        (
            format!(
                "#version {}\n{}",
                version,
                include_str!("glyph-vertex.glsl")
            ),
            format!("#version {}\n{}", version, include_str!("glyph-frag.glsl")),
        )
    }

    pub fn config_changed(&mut self, config: &ConfigHandle) {
        self.glyph_cache.borrow_mut().config_changed(config);
    }

    pub fn tier_swap_doctor_report(&self) -> TierSwapDoctorReport {
        self.glyph_cache.borrow().tier_swap_doctor_report()
    }

    pub fn recreate_texture_atlas(
        &mut self,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        size: Option<usize>,
    ) -> anyhow::Result<()> {
        // We make a a couple of passes at resizing; if the user has selected a large
        // font size (or a large scaling factor) then the `size==None` case will not
        // be able to fit the initial utility glyphs and apply_scale_change won't
        // be able to deal with that error situation.  Rather than make every
        // caller know how to deal with OutOfTextureSpace we try to absorb
        // and accomodate that here.
        let mut size = size;
        let mut attempt = 10;
        loop {
            match self.recreate_texture_atlas_impl(fonts, metrics, size) {
                Ok(_) => return Ok(()),
                Err(err) => {
                    attempt -= 1;
                    if attempt == 0 {
                        return Err(err);
                    }

                    if let Some(&OutOfTextureSpace {
                        size: Some(needed_size),
                        failure: AtlasAllocationFailure::Capacity,
                        ..
                    }) = err.downcast_ref::<OutOfTextureSpace>()
                    {
                        size.replace(needed_size);
                        continue;
                    }

                    return Err(err);
                }
            }
        }
    }

    fn recreate_texture_atlas_impl(
        &mut self,
        fonts: &Rc<FontConfiguration>,
        metrics: &RenderMetrics,
        size: Option<usize>,
    ) -> anyhow::Result<()> {
        let size = size.unwrap_or_else(|| self.glyph_cache.borrow().atlas.size());
        let mut new_glyph_cache = GlyphCache::new_gl(&self.context, fonts, size)?;
        self.util_sprites = UtilSprites::new(&mut new_glyph_cache, metrics)?;

        let mut glyph_cache = self.glyph_cache.borrow_mut();

        // Steal the complete decoded-image cache authority; without this,
        // animations reset and mutable-image ownership/accounting is lost
        // each time we fill the texture.
        glyph_cache.swap_decoded_image_cache_state(&mut new_glyph_cache);

        *glyph_cache = new_glyph_cache;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AtlasAllocationFailure, INDICES_PER_CELL, QUAD_CAPACITY_GROWTH_GRANULARITY,
        build_quad_indices, enforce_texture_atlas_budget, max_texture_atlas_side_for_budget,
        round_quad_capacity, texture_atlas_footprint_bytes,
    };
    use crate::quad::{V_BOT_LEFT, V_BOT_RIGHT, V_TOP_LEFT, V_TOP_RIGHT, VERTICES_PER_CELL};

    #[test]
    fn idle_quad_shrink_requires_stable_successful_geometry() {
        use super::{QuadShrinkUsage, quad_shrink_target};
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let small = QuadShrinkUsage::observe(None, 10, start, true).unwrap();
        assert!(!small.eligible(1024, start + Duration::from_millis(999)));
        assert!(small.eligible(1024, start + Duration::from_secs(1)));
        assert!(!small.eligible(128, start + Duration::from_secs(60)));
        let failed =
            QuadShrinkUsage::observe(Some(small), 0, start + Duration::from_secs(1), false);
        assert!(
            failed.is_none(),
            "partial cleared geometry is not a shrink baseline"
        );
        let recovered =
            QuadShrinkUsage::observe(failed, 10, start + Duration::from_secs(2), true).unwrap();
        assert!(!recovered.eligible(1024, start + Duration::from_secs(2)));
        let larger =
            QuadShrinkUsage::observe(Some(small), 400, start + Duration::from_secs(2), true)
                .unwrap();
        assert!(!larger.eligible(1024, start + Duration::from_secs(60)));
        assert_eq!(quad_shrink_target(usize::MAX), usize::MAX);
    }

    #[test]
    #[ignore = "requires a real WebGPU adapter; run explicitly for idle-shrink qualification"]
    fn webgpu_idle_shrink_reduces_owned_buffers_and_preserves_queued_geometry() {
        use super::*;
        use std::sync::mpsc;

        let (device, queue) = futures::executor::block_on(async {
            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: None,
                    force_fallback_adapter: false,
                    power_preference: wgpu::PowerPreference::LowPower,
                    apply_limit_buckets: false,
                })
                .await
                .expect("idle-shrink qualification requires an actual adapter");
            eprintln!("idle-shrink adapter: {:?}", adapter.get_info());
            adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
                .unwrap()
        });
        let owner = Rc::new(RefCell::new(std::array::from_fn(|_| {
            TripleVertexBuffer::new_webgpu(1024, &device, &queue)
        })));
        let owned_bytes = || {
            owner
                .borrow()
                .iter()
                .map(|buffer| {
                    buffer.indices.webgpu().buf.size()
                        + buffer
                            .bufs
                            .borrow()
                            .iter()
                            .map(|vertex| vertex.webgpu().buf.size())
                            .sum::<u64>()
                })
                .sum::<u64>()
        };
        let before_bytes = owned_bytes();
        let mut original_ids = Some(
            owner
                .borrow()
                .iter()
                .map(|buffer| buffer.bufs.borrow()[0].webgpu().buf.clone())
                .collect::<Vec<_>>(),
        );
        let prepare = || {
            (0..3)
                .map(|index| PreparedQuadShrink {
                    owner: Rc::clone(&owner),
                    index,
                    previous_capacity: 1024,
                    replacement: TripleVertexBuffer::new_webgpu(128, &device, &queue),
                })
                .collect::<Vec<_>>()
        };

        // Real backend validation refusal must leave the entire old batch intact.
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let invalid = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("deliberately invalid shrink allocation control"),
            size: device.limits().max_buffer_size + 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
        let failed_preparation = prepare();
        let rejected = futures::executor::block_on(finish_quad_shrink_allocation(vec![scope]));
        assert!(
            rejected.is_err(),
            "actual backend allocation validation must reject the batch"
        );
        drop(failed_preparation);
        drop(invalid);
        assert_eq!(owned_bytes(), before_bytes);

        let started = Instant::now();
        for (now, activity, resizing) in [
            (started, started, true),
            (
                started + Duration::from_millis(1),
                started + Duration::from_millis(1),
                false,
            ),
            (started + Duration::from_secs(6), started, false),
        ] {
            let mut pending = PendingQuadShrink {
                activity: started,
                started,
                prepared: Some(Ok(prepare())),
                ready: finish_quad_shrink_allocation(Vec::new()),
            };
            pending.cancel_if_stale(now, activity, resizing);
            assert!(
                pending.prepared.is_none(),
                "interaction/deadline must release replacement resources"
            );
            futures::executor::block_on(pending.ready).unwrap();
            assert_eq!(owned_bytes(), before_bytes);
        }
        for (index, original) in original_ids.as_ref().unwrap().iter().enumerate() {
            assert_eq!(
                &owner.borrow()[index].bufs.borrow()[0].webgpu().buf,
                original
            );
        }

        // A still-borrowed owner cannot partially commit a prepared batch.
        let retained_mapping = owner.borrow();
        assert!(commit_quad_shrink(prepare()).is_err());
        drop(retained_mapping);
        assert_eq!(owned_bytes(), before_bytes);

        let expected: Vec<_> = (0..8)
            .map(|index| Vertex {
                position: [index as f32, 7.0],
                fg_color: [0.25, 0.5, 0.75, 1.0],
                ..Vertex::default()
            })
            .collect();
        let expected_bytes = bytemuck::cast_slice::<_, u8>(&expected).to_vec();
        let mut readbacks = Vec::new();
        // Submit the old frame before replacing its owners. No GPU completion
        // wait separates old submission, replacement, and new submission.
        for phase in 0..2 {
            if phase == 1 {
                let scopes = vec![
                    device.push_error_scope(wgpu::ErrorFilter::OutOfMemory),
                    device.push_error_scope(wgpu::ErrorFilter::Internal),
                    device.push_error_scope(wgpu::ErrorFilter::Validation),
                ];
                let prepared = prepare();
                futures::executor::block_on(finish_quad_shrink_allocation(scopes)).unwrap();
                assert_eq!(commit_quad_shrink(prepared).unwrap(), 3);
                assert_eq!(owned_bytes(), before_bytes / 8);
                for (index, original) in original_ids.take().unwrap().iter().enumerate() {
                    assert_ne!(
                        &owner.borrow()[index].bufs.borrow()[0].webgpu().buf,
                        original
                    );
                }
            }
            for buffer in owner.borrow().iter() {
                buffer.clear_quad_allocation();
                for quad in expected.chunks_exact(VERTICES_PER_CELL) {
                    buffer.map().extend_with(quad);
                }
                let vertex_count = buffer.vertex_index_count().0;
                assert_eq!(vertex_count, expected.len());
                let uploaded = buffer.current_vb_mut().webgpu().upload(vertex_count);
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("idle-shrink queued geometry readback"),
                    size: expected_bytes.len() as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut encoder =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                encoder.copy_buffer_to_buffer(
                    &uploaded,
                    0,
                    &readback,
                    0,
                    expected_bytes.len() as u64,
                );
                queue.submit([encoder.finish()]);
                readbacks.push(readback);
                buffer.next_index();
            }
        }
        assert!(
            original_ids.is_none(),
            "old owner handles must drop before waiting for GPU work"
        );
        for readback in readbacks {
            let slice = readback.slice(..);
            let (sender, receiver) = mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).unwrap()
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                device.poll(wgpu::PollType::Poll).unwrap();
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => {
                        result.unwrap();
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                    other => panic!("queued shrink readback did not complete: {other:?}"),
                }
            }
            let bytes = slice.get_mapped_range().unwrap();
            assert_eq!(&*bytes, expected_bytes.as_slice());
            drop(bytes);
            readback.unmap();
        }
    }

    #[test]
    #[ignore = "requires a real WebGPU adapter; run explicitly for vertex-buffer qualification"]
    fn webgpu_vertex_upload_reuses_buffers_and_preserves_queued_frames() {
        use super::{
            IndexBuffer, TripleVertexBuffer, Vertex, VertexBuffer, WebGpuGlyphQuadSoaStaging,
            WebGpuIndexBuffer, WebGpuVertexBuffer,
        };
        use crate::quad::QuadAllocator;
        use std::cell::RefCell;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use wgpu::util::DeviceExt;

        let (device, queue) = futures::executor::block_on(async {
            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: None,
                    force_fallback_adapter: false,
                    power_preference: wgpu::PowerPreference::LowPower,
                    apply_limit_buckets: false,
                })
                .await
                .expect("vertex qualification requires an actual adapter");
            eprintln!("vertex upload adapter: {:?}", adapter.get_info());
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("vertex upload qualification"),
                    ..Default::default()
                })
                .await
                .expect("create vertex qualification device")
        });
        let buffers = TripleVertexBuffer {
            index: RefCell::new(0),
            bufs: RefCell::new(std::array::from_fn(|_| {
                VertexBuffer::WebGpu(WebGpuVertexBuffer::new(8, &device, &queue))
            })),
            glyph_quad_instances: RefCell::new(std::array::from_fn(|_| {
                WebGpuGlyphQuadSoaStaging::default()
            })),
            indices: IndexBuffer::WebGpu(WebGpuIndexBuffer {
                buf: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex qualification indices"),
                    usage: wgpu::BufferUsages::INDEX,
                    contents: bytemuck::cast_slice(&build_quad_indices(2)),
                }),
            }),
            capacity: 2,
            next_quad: RefCell::new(0),
        };
        let identities: Vec<_> = buffers
            .bufs
            .borrow()
            .iter()
            .map(|buffer| buffer.webgpu().buf.clone())
            .collect();
        let glyph = WebGpuGlyphQuadSoaStaging {
            positions: vec![[1.0, 2.0, 3.0, 4.0]],
            tex_rects: vec![[0.0, 0.0, 1.0, 1.0]],
            fg_colors: vec![[1.0; 4]],
            alt_colors: vec![[0.0; 4]],
            hsv: vec![[1.0; 3]],
            has_color: vec![0.0],
            mix_values: vec![0.0],
        };
        buffers.map().extend_with_glyph_quad_soa(glyph.buffers());
        buffers.map().extend_with_glyph_quad_soa(glyph.buffers());
        assert_eq!(
            buffers.current_glyph_quad_instances().len(),
            2,
            "borrowing another allocator must preserve earlier glyphs this frame"
        );
        buffers.clear_quad_allocation();
        assert!(buffers.current_glyph_quad_instances().is_empty());
        let mut readbacks = Vec::new();
        // Reuse every triple-buffer slot after both full and empty frames.
        // Submit all copies before waiting so later CPU writes cannot hide a
        // broken ownership or queue-ordering contract by serializing the test.
        for (frame, quad_count) in [2, 1, 0, 1, 2, 2, 0, 1].into_iter().enumerate() {
            buffers.clear_quad_allocation();
            let expected: Vec<_> = (0..quad_count * VERTICES_PER_CELL)
                .map(|vertex| Vertex {
                    position: [frame as f32, vertex as f32],
                    fg_color: [0.25, 0.5, 0.75, 1.0],
                    mix_value: frame as f32 / 8.0,
                    ..Vertex::default()
                })
                .collect();
            for (quad, vertices) in expected.chunks_exact(VERTICES_PER_CELL).enumerate() {
                let mut mapped = buffers.map();
                let start = quad * VERTICES_PER_CELL;
                assert_eq!(
                    mapped.mapping.slice_mut(start..start + VERTICES_PER_CELL),
                    &[Vertex::default(); VERTICES_PER_CELL],
                    "fresh quads must not inherit an earlier frame"
                );
                mapped.extend_with(vertices);
            }
            let (vertex_count, _) = buffers.vertex_index_count();
            assert_eq!(vertex_count, expected.len());
            let buffer = buffers.current_vb_mut().webgpu().upload(vertex_count);
            assert_eq!(buffer, identities[frame % 3], "GPU buffer must be reused");
            if vertex_count > 0 {
                let expected_bytes: Vec<u8> = bytemuck::cast_slice(&expected).to_vec();
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vertex qualification readback"),
                    size: expected_bytes.len() as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut encoder =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                encoder.copy_buffer_to_buffer(
                    &buffer,
                    0,
                    &readback,
                    0,
                    expected_bytes.len() as u64,
                );
                queue.submit([encoder.finish()]);
                readbacks.push((readback, expected_bytes));
            }
            buffers.next_index();
        }
        for (readback, expected) in readbacks {
            let slice = readback.slice(..);
            let (sender, receiver) = mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                sender.send(result).expect("readback receiver is alive");
            });
            let started = Instant::now();
            loop {
                device.poll(wgpu::PollType::Poll).expect("poll actual GPU");
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(result) => {
                        result.expect("map completed GPU copy");
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if started.elapsed() < Duration::from_secs(30) => {}
                    other => panic!("GPU readback did not complete: {other:?}"),
                }
            }
            let actual = slice
                .get_mapped_range()
                .expect("access mapped vertex readback");
            assert_eq!(&*actual, expected.as_slice(), "queued frame bytes changed");
            drop(actual);
            readback.unmap();
        }
    }

    #[test]
    fn build_quad_indices_for_zero_quads_is_empty() {
        assert!(build_quad_indices(0).is_empty());
    }

    #[test]
    fn build_quad_indices_for_single_quad_matches_expected_triangles() {
        assert_eq!(
            build_quad_indices(1),
            vec![
                V_TOP_LEFT as u32,
                V_TOP_RIGHT as u32,
                V_BOT_LEFT as u32,
                V_TOP_RIGHT as u32,
                V_BOT_RIGHT as u32,
                V_BOT_LEFT as u32,
            ]
        );
    }

    #[test]
    fn build_quad_indices_offset_each_quad_by_vertex_stride() {
        let indices = build_quad_indices(3);
        assert_eq!(indices.len(), 3 * INDICES_PER_CELL);

        for (quad_idx, chunk) in indices.chunks_exact(INDICES_PER_CELL).enumerate() {
            let base = (quad_idx * VERTICES_PER_CELL) as u32;
            assert_eq!(
                chunk,
                &[
                    base + V_TOP_LEFT as u32,
                    base + V_TOP_RIGHT as u32,
                    base + V_BOT_LEFT as u32,
                    base + V_TOP_RIGHT as u32,
                    base + V_BOT_RIGHT as u32,
                    base + V_BOT_LEFT as u32,
                ]
            );
        }
    }

    #[test]
    fn round_quad_capacity_uses_growth_granularity() {
        assert_eq!(round_quad_capacity(0), 0);
        assert_eq!(round_quad_capacity(1), QUAD_CAPACITY_GROWTH_GRANULARITY);
        assert_eq!(round_quad_capacity(127), QUAD_CAPACITY_GROWTH_GRANULARITY);
        assert_eq!(round_quad_capacity(128), QUAD_CAPACITY_GROWTH_GRANULARITY);
        assert_eq!(
            round_quad_capacity(129),
            QUAD_CAPACITY_GROWTH_GRANULARITY * 2
        );
        assert_eq!(
            round_quad_capacity(257),
            QUAD_CAPACITY_GROWTH_GRANULARITY * 3
        );
    }

    #[test]
    fn texture_atlas_budget_caps_default_vram_side() {
        let budget = 256 * 1024 * 1024;
        let side = max_texture_atlas_side_for_budget(budget);

        assert_eq!(side, 8192);
        assert_eq!(texture_atlas_footprint_bytes(side), budget);
        assert!(texture_atlas_footprint_bytes(side * 2) > budget);
    }

    #[test]
    fn texture_atlas_budget_rejects_one_gib_atlas_growth() {
        let err = enforce_texture_atlas_budget(16_384)
            .expect_err("1 GiB atlas should exceed the default VRAM budget");

        assert_eq!(err.size, None);
        assert_eq!(err.current_size, 8192);
        assert_eq!(err.failure, AtlasAllocationFailure::MemoryBudget);
    }
}
