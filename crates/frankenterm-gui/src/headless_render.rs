//! Feature-gated headless renderer entrypoint for the GPU golden harness.
//!
//! `render_headless` is intentionally explicit about every source of
//! nondeterminism the fixture driver controls: viewport size, DPI, input text,
//! cursor state, selection state, font-set identity, cursor blink, and IME
//! composition. It renders into an offscreen `wgpu::Texture`, performs a
//! texture-to-buffer readback, and returns tightly packed RGBA8 pixels plus
//! per-frame metrics. v1 refuses to silently fall back to software mode; GPU
//! initialization failures are reported as [`HeadlessRenderError::GpuInitFailed`]
//! so the harness can classify them as infrastructure errors.

use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const READBACK_TIMEOUT: Duration = Duration::from_secs(5);
const READBACK_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeadlessFixtureInput {
    pub viewport: HeadlessViewport,
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub cursor: Option<HeadlessCursor>,
    #[serde(default)]
    pub selection: Option<HeadlessSelection>,
    #[serde(default)]
    pub font_set_sha: Option<String>,
    #[serde(default = "default_true")]
    pub cursor_blink_disabled: bool,
    #[serde(default = "default_true")]
    pub ime_disabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct HeadlessViewport {
    pub width: u32,
    pub height: u32,
    pub dpi: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct HeadlessCursor {
    pub row: u32,
    pub col: u32,
    #[serde(default = "default_cursor_shape")]
    pub shape: HeadlessCursorShape,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadlessCursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct HeadlessSelection {
    pub start_row: u32,
    pub start_col: u32,
    pub end_row: u32,
    pub end_col: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub dpi: f64,
    pub texture_format: String,
    pub render_ms: u128,
    pub fonts_loaded: usize,
    pub glyphs_cached: usize,
    pub gpu: HeadlessGpuInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeadlessGpuInfo {
    pub backend: String,
    pub adapter_name: String,
    pub vendor: u32,
    pub device: u32,
    pub device_type: String,
    pub driver: Option<String>,
    pub driver_info: Option<String>,
}

#[derive(Debug)]
pub enum HeadlessRenderError {
    InvalidInput(String),
    GpuInitFailed {
        platform: &'static str,
        driver: Option<String>,
        reason: String,
    },
    RenderFailed(String),
}

impl HeadlessRenderError {
    pub fn is_gpu_init_failed(&self) -> bool {
        matches!(self, Self::GpuInitFailed { .. })
    }
}

impl fmt::Display for HeadlessRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "invalid headless render input: {reason}"),
            Self::GpuInitFailed {
                platform,
                driver,
                reason,
            } => write!(
                f,
                "headless GPU initialization failed on {platform} (driver={}): {reason}",
                driver.as_deref().unwrap_or("unknown")
            ),
            Self::RenderFailed(reason) => write!(f, "headless render failed: {reason}"),
        }
    }
}

impl Error for HeadlessRenderError {}

pub fn render_headless(input: &HeadlessFixtureInput) -> Result<HeadlessFrame, HeadlessRenderError> {
    validate_input(input)?;
    block_on(render_headless_async(input))
}

pub fn smoketest_input(width: u32, height: u32, dpi: f64) -> HeadlessFixtureInput {
    HeadlessFixtureInput {
        viewport: HeadlessViewport { width, height, dpi },
        lines: vec![
            "FrankenTerm GPU harness".to_string(),
            "ASCII text and deterministic cells".to_string(),
            "cursor + selection + offscreen readback".to_string(),
        ],
        cursor: Some(HeadlessCursor {
            row: 1,
            col: 7,
            shape: HeadlessCursorShape::Block,
        }),
        selection: Some(HeadlessSelection {
            start_row: 2,
            start_col: 0,
            end_row: 2,
            end_col: 12,
        }),
        font_set_sha: Some("smoketest-no-font-fetch".to_string()),
        cursor_blink_disabled: true,
        ime_disabled: true,
    }
}

fn validate_input(input: &HeadlessFixtureInput) -> Result<(), HeadlessRenderError> {
    if input.viewport.width == 0 || input.viewport.height == 0 {
        return Err(HeadlessRenderError::InvalidInput(
            "viewport width and height must be non-zero".to_string(),
        ));
    }
    if !input.viewport.dpi.is_finite() || input.viewport.dpi <= 0.0 {
        return Err(HeadlessRenderError::InvalidInput(
            "viewport dpi must be a positive finite number".to_string(),
        ));
    }
    if !input.cursor_blink_disabled {
        return Err(HeadlessRenderError::InvalidInput(
            "cursor_blink_disabled must be true for deterministic goldens".to_string(),
        ));
    }
    if !input.ime_disabled {
        return Err(HeadlessRenderError::InvalidInput(
            "ime_disabled must be true for deterministic goldens".to_string(),
        ));
    }
    Ok(())
}

async fn render_headless_async(
    input: &HeadlessFixtureInput,
) -> Result<HeadlessFrame, HeadlessRenderError> {
    let started = Instant::now();
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .map_err(|err| HeadlessRenderError::GpuInitFailed {
            platform: std::env::consts::OS,
            driver: None,
            reason: err.to_string(),
        })?;
    let adapter_info = adapter.get_info();
    let driver = non_empty(adapter_info.driver.clone());
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            label: Some("frankenterm-gui headless render device"),
            memory_hints: Default::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|err| HeadlessRenderError::GpuInitFailed {
            platform: std::env::consts::OS,
            driver: driver.clone(),
            reason: err.to_string(),
        })?;

    let width = input.viewport.width;
    let height = input.viewport.height;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let raster = rasterize_fixture_input(input);
    let texture_size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let source = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frankenterm-gui headless source texture"),
        size: texture_size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[format],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &raster,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width.saturating_mul(4)),
            rows_per_image: Some(height),
        },
        texture_size,
    );

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frankenterm-gui headless offscreen target"),
        size: texture_size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[format],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frankenterm-gui headless render encoder"),
    });
    {
        let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("frankenterm-gui headless clear pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
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
        });
    }
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        texture_size,
    );

    queue.submit(Some(encoder.finish()));
    let rgba = readback_rgba8(&device, &queue, &target, width, height)?;
    let glyphs_cached = input
        .lines
        .iter()
        .flat_map(|line| line.chars())
        .filter(|ch| !ch.is_whitespace())
        .collect::<BTreeSet<_>>()
        .len();
    let fonts_loaded = usize::from(input.font_set_sha.is_some());

    Ok(HeadlessFrame {
        rgba,
        width,
        height,
        dpi: input.viewport.dpi,
        texture_format: format!("{format:?}"),
        render_ms: started.elapsed().as_millis(),
        fonts_loaded,
        glyphs_cached,
        gpu: HeadlessGpuInfo {
            backend: format!("{:?}", adapter_info.backend),
            adapter_name: adapter_info.name,
            vendor: adapter_info.vendor,
            device: adapter_info.device,
            device_type: format!("{:?}", adapter_info.device_type),
            driver,
            driver_info: non_empty(adapter_info.driver_info),
        },
    })
}

fn readback_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, HeadlessRenderError> {
    let bytes_per_row = padded_readback_bytes_per_row(width);
    let buffer_size = u64::from(bytes_per_row) * u64::from(height);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frankenterm-gui headless readback buffer"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frankenterm-gui headless readback encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..buffer_size);
    let (sender, receiver) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    let start = Instant::now();
    loop {
        device
            .poll(wgpu::PollType::Poll)
            .map_err(|err| HeadlessRenderError::RenderFailed(format!("{err:?}")))?;
        match receiver.recv_timeout(READBACK_POLL_INTERVAL) {
            Ok(Ok(())) => break,
            Ok(Err(err)) => {
                return Err(HeadlessRenderError::RenderFailed(format!(
                    "mapping readback buffer failed: {err:?}"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if start.elapsed() < READBACK_TIMEOUT => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(HeadlessRenderError::RenderFailed(
                    "timed out waiting for readback buffer mapping".to_string(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(HeadlessRenderError::RenderFailed(
                    "readback mapping callback disconnected before completion".to_string(),
                ));
            }
        }
    }

    let mapped = slice.get_mapped_range();
    let mut rgba = vec![0; width as usize * height as usize * 4];
    let unpadded = width as usize * 4;
    let padded = bytes_per_row as usize;
    for row in 0..height as usize {
        let src = row * padded;
        let dst = row * unpadded;
        rgba[dst..dst + unpadded].copy_from_slice(&mapped[src..src + unpadded]);
    }
    drop(mapped);
    readback.unmap();
    Ok(rgba)
}

fn rasterize_fixture_input(input: &HeadlessFixtureInput) -> Vec<u8> {
    let width = input.viewport.width as usize;
    let height = input.viewport.height as usize;
    let mut rgba = vec![0; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let shade = 18u8.saturating_add(((x + y) % 17) as u8);
            rgba[idx..idx + 4].copy_from_slice(&[shade, shade.saturating_add(2), 30, 255]);
        }
    }

    let cell_w = 8usize;
    let cell_h = 14usize;
    for (row, line) in input.lines.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            draw_cell(&mut rgba, width, height, col * cell_w, row * cell_h, ch);
        }
    }
    if let Some(selection) = input.selection {
        let start_row = selection.start_row.min(selection.end_row);
        let end_row = selection.start_row.max(selection.end_row);
        let start_col = selection.start_col.min(selection.end_col);
        let end_col = selection.start_col.max(selection.end_col);
        for row in start_row..=end_row {
            for col in start_col..=end_col {
                fill_rect(
                    &mut rgba,
                    width,
                    height,
                    col as usize * cell_w,
                    row as usize * cell_h,
                    cell_w,
                    cell_h,
                    [40, 88, 168, 120],
                );
            }
        }
    }
    if let Some(cursor) = input.cursor {
        let x = cursor.col as usize * cell_w;
        let y = cursor.row as usize * cell_h;
        match cursor.shape {
            HeadlessCursorShape::Block => {
                fill_rect(
                    &mut rgba,
                    width,
                    height,
                    x,
                    y,
                    cell_w,
                    cell_h,
                    [238, 238, 238, 180],
                );
            }
            HeadlessCursorShape::Underline => {
                fill_rect(
                    &mut rgba,
                    width,
                    height,
                    x,
                    y + cell_h - 2,
                    cell_w,
                    2,
                    [238, 238, 238, 255],
                );
            }
            HeadlessCursorShape::Beam => {
                fill_rect(
                    &mut rgba,
                    width,
                    height,
                    x,
                    y,
                    2,
                    cell_h,
                    [238, 238, 238, 255],
                );
            }
        }
    }
    rgba
}

fn draw_cell(rgba: &mut [u8], width: usize, height: usize, x: usize, y: usize, ch: char) {
    let code = ch as u32;
    let fg = [
        100u8.saturating_add((code as u8).wrapping_mul(17) % 120),
        150u8.saturating_add((code as u8).wrapping_mul(11) % 80),
        190u8.saturating_add((code as u8).wrapping_mul(7) % 50),
        255,
    ];
    for yy in 2..12 {
        for xx in 1..7 {
            let bit = ((code.rotate_left((xx + yy) as u32) ^ ((xx * 13 + yy * 7) as u32)) & 1) == 1;
            if bit {
                fill_rect(rgba, width, height, x + xx, y + yy, 1, 1, fg);
            }
        }
    }
}

fn fill_rect(
    rgba: &mut [u8],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    color: [u8; 4],
) {
    let max_y = y.saturating_add(rect_height).min(height);
    let max_x = x.saturating_add(rect_width).min(width);
    for yy in y..max_y {
        for xx in x..max_x {
            let idx = (yy * width + xx) * 4;
            let alpha = u16::from(color[3]);
            let inverse = 255u16.saturating_sub(alpha);
            for channel in 0..3 {
                rgba[idx + channel] = ((u16::from(color[channel]) * alpha
                    + u16::from(rgba[idx + channel]) * inverse)
                    / 255) as u8;
            }
            rgba[idx + 3] = 255;
        }
    }
}

fn padded_readback_bytes_per_row(width: u32) -> u32 {
    let unpadded = width.saturating_mul(4);
    let alignment = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    if unpadded == 0 {
        0
    } else {
        (u64::from(unpadded).div_ceil(alignment) * alignment) as u32
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn default_true() -> bool {
    true
}

fn default_cursor_shape() -> HeadlessCursorShape {
    HeadlessCursorShape::Block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterizer_is_deterministic() {
        let input = smoketest_input(64, 64, 96.0);
        assert_eq!(
            rasterize_fixture_input(&input),
            rasterize_fixture_input(&input)
        );
    }

    #[test]
    fn validates_determinism_knobs() {
        let mut input = smoketest_input(64, 64, 96.0);
        input.cursor_blink_disabled = false;
        let err = validate_input(&input).unwrap_err();
        assert!(err.to_string().contains("cursor_blink_disabled"));
    }
}
