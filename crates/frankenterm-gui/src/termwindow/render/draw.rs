use crate::colorease::ColorEaseUniform;
use crate::termwindow::webgpu::{AcquiredWebGpuFrame, ShaderUniform};
use crate::uniforms::UniformBuilder;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use anyhow::{Context, anyhow};
use config::FreeTypeLoadTarget;

/// The renderer stage that rejected a draw before it could be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawFailureStage {
    RenderCommands,
    MissingGlyphProgram,
    BufferSliceBounds,
    BackendDraw,
}

impl DrawFailureStage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::RenderCommands => "render_commands",
            Self::MissingGlyphProgram => "missing_glyph_program",
            Self::BufferSliceBounds => "buffer_slice_bounds",
            Self::BackendDraw => "backend_draw",
        }
    }
}

/// Typed boundary error used for failures after geometry construction.
#[derive(Debug)]
pub(crate) struct DrawFailure {
    stage: DrawFailureStage,
    source: anyhow::Error,
}

impl DrawFailure {
    pub(crate) fn new(stage: DrawFailureStage, source: anyhow::Error) -> Self {
        Self { stage, source }
    }

    pub(crate) const fn stage(&self) -> DrawFailureStage {
        self.stage
    }
}

impl std::fmt::Display for DrawFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} draw failure: {:#}", self.stage.label(), self.source)
    }
}

impl std::error::Error for DrawFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl crate::TermWindow {
    pub(crate) fn call_draw_webgpu(
        &mut self,
        acquired: AcquiredWebGpuFrame,
    ) -> anyhow::Result<()> {
        use crate::termwindow::webgpu::WebGpuTexture;

        let webgpu = self
            .webgpu
            .as_mut()
            .context("webgpu state is not initialized")?;
        let render_state = self
            .render_state
            .as_ref()
            .context("render state is not initialized")?;

        if acquired.suboptimal {
            log::warn!(
                "webgpu surface texture is suboptimal; presenting it before forced reconfigure"
            );
        }
        let output = acquired.texture;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = webgpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });
        let tex = render_state.glyph_cache.borrow().atlas.texture();
        let tex = tex
            .downcast_ref::<WebGpuTexture>()
            .context("glyph atlas is not a WebGPU texture")?;
        let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let texture_linear_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_linear_sampler),
                    },
                ],
                label: Some("linear bind group"),
            });

        let texture_nearest_bind_group =
            webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &webgpu.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&webgpu.texture_nearest_sampler),
                    },
                ],
                label: Some("nearest bind group"),
            });

        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = [
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        ];

        let milliseconds = self.created.elapsed().as_millis() as u32;
        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        webgpu.update_uniform(ShaderUniform {
            foreground_text_hsb,
            milliseconds,
            projection,
        });
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.,
                        g: 0.,
                        b: 0.,
                        a: 0.,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
            multiview_mask: None,
        });
        render_pass.set_pipeline(&webgpu.render_pipeline);
        render_pass.set_bind_group(0, webgpu.shader_uniform_bind_group(), &[]);
        render_pass.set_bind_group(1, &texture_linear_bind_group, &[]);
        render_pass.set_bind_group(2, &texture_nearest_bind_group, &[]);

        for layer in render_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                {
                    let glyph_instances = vb.current_glyph_quad_instances();
                    if !glyph_instances.is_empty() {
                        if let (Some(glyph_quad_instance_render_pipeline), Some(instance_buffers)) = (
                            webgpu.glyph_quad_instance_render_pipeline.as_ref(),
                            webgpu.create_glyph_quad_instance_buffers(glyph_instances.buffers()),
                        ) {
                            render_pass.set_pipeline(glyph_quad_instance_render_pipeline);
                            render_pass.set_vertex_buffer(0, instance_buffers.positions.slice(..));
                            render_pass.set_vertex_buffer(1, instance_buffers.tex_rects.slice(..));
                            render_pass.set_vertex_buffer(2, instance_buffers.fg_colors.slice(..));
                            render_pass.set_vertex_buffer(3, instance_buffers.alt_colors.slice(..));
                            render_pass.set_vertex_buffer(4, instance_buffers.hsv.slice(..));
                            render_pass.set_vertex_buffer(5, instance_buffers.has_color.slice(..));
                            render_pass.set_vertex_buffer(6, instance_buffers.mix_values.slice(..));
                            render_pass.draw(0..4, 0..instance_buffers.instance_count);
                            render_pass.set_pipeline(&webgpu.render_pipeline);
                        }
                    }
                }

                let (vertex_count, index_count) = vb.vertex_index_count();
                if vertex_count > 0 {
                    let mut vertices = vb.current_vb_mut();
                    let vertex_buffer = vertices.webgpu_mut().recreate();
                    vertex_buffer.unmap();
                    render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                    render_pass
                        .set_index_buffer(vb.indices.webgpu().slice(..), wgpu::IndexFormat::Uint32);
                    render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                }

                vb.next_index();
            }
        }
        drop(render_pass);

        // In this wgpu API both `Queue::submit` and `SurfaceTexture::present`
        // are synchronously infallible. Device-loss errors reported later by
        // wgpu are outside this synchronous seam; a successful return does not
        // prove asynchronous GPU completion or visible scanout.
        let _submission = webgpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok(())
    }

    pub(crate) fn call_draw_glium(&mut self, frame: &mut glium::Frame) -> anyhow::Result<()> {
        use window::glium::texture::SrgbTexture2d;

        let gl_state = self
            .render_state
            .as_ref()
            .context("render state is not initialized")?;
        let tex = gl_state.glyph_cache.borrow().atlas.texture();
        let tex = tex
            .downcast_ref::<SrgbTexture2d>()
            .context("glyph atlas is not a glium SrgbTexture2d")?;
        let prog = gl_state.glyph_prog.as_ref().ok_or_else(|| {
            DrawFailure::new(
                DrawFailureStage::MissingGlyphProgram,
                anyhow!("glyph program is not initialized"),
            )
        })?;

        frame.clear_color(0., 0., 0., 0.);

        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        let use_subpixel = match self
            .config
            .freetype_render_target
            .unwrap_or(self.config.freetype_load_target)
        {
            FreeTypeLoadTarget::HorizontalLcd | FreeTypeLoadTarget::VerticalLcd => true,
            _ => false,
        };

        let dual_source_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },

            ..Default::default()
        };

        let alpha_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceAlpha,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::One,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },
            ..Default::default()
        };

        // Clamp and use the nearest texel rather than interpolate.
        // This prevents things like the box cursor outlines from
        // being randomly doubled in width or height
        let atlas_nearest_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest);

        let atlas_linear_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);

        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = (
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        );

        let milliseconds = self.created.elapsed().as_millis() as u32;

        let cursor_blink: ColorEaseUniform = (*self.cursor_blink_state.borrow()).into();
        let blink: ColorEaseUniform = (*self.blink_state.borrow()).into();
        let rapid_blink: ColorEaseUniform = (*self.rapid_blink_state.borrow()).into();

        let mut draw_failure = None;
        for layer in gl_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let result = if draw_failure.is_some() {
                    Ok(())
                } else {
                    (|| -> anyhow::Result<()> {
                        let (vertex_count, index_count) = vb.vertex_index_count();
                        if vertex_count == 0 {
                            return Ok(());
                        }

                        let vertices = vb.current_vb_mut();
                        let subpixel_aa = use_subpixel && idx == 1;

                        let mut uniforms = UniformBuilder::default();

                        uniforms.add("projection", &projection);
                        uniforms.add("atlas_nearest_sampler", &atlas_nearest_sampler);
                        uniforms.add("atlas_linear_sampler", &atlas_linear_sampler);
                        uniforms.add("foreground_text_hsb", &foreground_text_hsb);
                        uniforms.add("subpixel_aa", &subpixel_aa);
                        uniforms.add("milliseconds", &milliseconds);
                        uniforms.add_struct("cursor_blink", &cursor_blink);
                        uniforms.add_struct("blink", &blink);
                        uniforms.add_struct("rapid_blink", &rapid_blink);

                        // A missing slice means the logical quad accounting and
                        // physical GL buffers disagree. Advance every
                        // triple-buffer cursor below, but report the invariant
                        // separately from a missing shader program so recovery
                        // telemetry identifies the exact reconstruction seam.
                        let vertex_buf_len = vertices.glium().len();
                        let index_buf_len = vb.indices.glium().len();
                        let vert_slice = vertices.glium().slice(0..vertex_count).ok_or_else(|| {
                            DrawFailure::new(
                                DrawFailureStage::BufferSliceBounds,
                                anyhow!(
                                    "vertex slice 0..{vertex_count} exceeds buffer length {vertex_buf_len}"
                                ),
                            )
                        })?;
                        let idx_slice = vb.indices.glium().slice(0..index_count).ok_or_else(|| {
                            DrawFailure::new(
                                DrawFailureStage::BufferSliceBounds,
                                anyhow!(
                                    "index slice 0..{index_count} exceeds buffer length {index_buf_len}"
                                ),
                            )
                        })?;
                        frame
                            .draw(
                                vert_slice,
                                idx_slice,
                                prog,
                                &uniforms,
                                if subpixel_aa {
                                    &dual_source_blending
                                } else {
                                    &alpha_blending
                                },
                            )
                            .map_err(|err| {
                                DrawFailure::new(DrawFailureStage::BackendDraw, err.into())
                            })?;
                        Ok(())
                    })()
                };

                vb.next_index();
                if let Err(err) = result {
                    draw_failure = Some(err);
                }
            }
        }

        match draw_failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
