//! Render snapshots for the renderer image-parity corpus (ft-yccm0.1.10).
//!
//! When `FRANKENTERM_RENDER_SNAPSHOT` names a PNG path, the first frame the
//! active front end presents after the active pane's title becomes the
//! sentinel (`FRANKENTERM_RENDER_SNAPSHOT_TITLE`, default
//! `ft-render-snapshot`) is read back from the GPU and written there. A scene
//! prints its bytes and then sets that title (`OSC 2`), and terminal output
//! is applied in order, so the snapshot holds the whole scene as the real
//! renderer drew it: fonts, shaping, glyph atlas, shaders and chrome.
//!
//! The variables are read once at window creation; with neither set this
//! module does nothing. A snapshot is taken once per window and never
//! affects the presented frame.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const SNAPSHOT_PATH_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT";
pub(crate) const SNAPSHOT_TITLE_ENV: &str = "FRANKENTERM_RENDER_SNAPSHOT_TITLE";
pub(crate) const DEFAULT_SNAPSHOT_TITLE: &str = "ft-render-snapshot";
const READBACK_TIMEOUT: Duration = Duration::from_secs(10);
const READBACK_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A pending one-shot snapshot request for one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderSnapshotRequest {
    path: PathBuf,
    title: String,
    taken: bool,
}

impl RenderSnapshotRequest {
    pub(crate) fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var_os(SNAPSHOT_PATH_ENV),
            std::env::var(SNAPSHOT_TITLE_ENV).ok(),
        )
    }

    fn from_values(path: Option<std::ffi::OsString>, title: Option<String>) -> Option<Self> {
        let path = PathBuf::from(path.filter(|path| !path.is_empty())?);
        let title = title
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| DEFAULT_SNAPSHOT_TITLE.to_string());
        Some(Self {
            path,
            title,
            taken: false,
        })
    }

    /// Whether the WebGPU surface must be configured readable (`COPY_SRC`).
    pub(crate) fn requested() -> bool {
        std::env::var_os(SNAPSHOT_PATH_ENV).is_some_and(|path| !path.is_empty())
    }

    /// Claims the snapshot if the active pane's title is the sentinel. Returns
    /// the output path at most once.
    pub(crate) fn claim(&mut self, active_title: Option<&str>) -> Option<PathBuf> {
        if self.taken || active_title != Some(self.title.as_str()) {
            return None;
        }
        self.taken = true;
        Some(self.path.clone())
    }
}

/// Converts tightly packed texels of `format` into RGBA8. Only the 8-bit
/// four-channel formats a presentable surface uses are accepted.
pub(crate) fn texels_to_rgba8(
    format: wgpu::TextureFormat,
    mut texels: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    use wgpu::TextureFormat as F;
    match format {
        F::Rgba8Unorm | F::Rgba8UnormSrgb => Ok(texels),
        F::Bgra8Unorm | F::Bgra8UnormSrgb => {
            for pixel in texels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            Ok(texels)
        }
        other => anyhow::bail!("render snapshot cannot read surface format {other:?}"),
    }
}

/// Writes an RGBA8 PNG via a temporary file and a rename, so a reader polling
/// for `path` never sees a partial image.
pub(crate) fn write_png_atomically(
    path: &Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> anyhow::Result<()> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .with_context(|| format!("snapshot buffer does not hold {width}x{height} RGBA8 pixels"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    image
        .save_with_format(&partial, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path)
        .with_context(|| format!("renaming {} to {}", partial.display(), path.display()))?;
    log::info!(
        "render snapshot written: {} ({width}x{height})",
        path.display()
    );
    Ok(())
}

/// A texture-to-buffer copy recorded into a frame's encoder, read back after
/// that frame is submitted.
pub(crate) struct PendingReadback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    format: wgpu::TextureFormat,
    path: PathBuf,
}

impl PendingReadback {
    /// Records a copy of `texture` (which must have `COPY_SRC` usage) into
    /// `encoder`, ahead of the frame's submission.
    pub(crate) fn record(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            texture.usage().contains(wgpu::TextureUsages::COPY_SRC),
            "surface texture was not configured with COPY_SRC; is {SNAPSHOT_PATH_ENV} set before launch?"
        );
        let size = texture.size();
        let (width, height) = (size.width, size.height);
        let unpadded = width.checked_mul(4).context("snapshot width overflows")?;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded.div_ceil(align) * align;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frankenterm-gui render snapshot readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        Ok(Self {
            buffer,
            width,
            height,
            padded_bytes_per_row,
            format: texture.format(),
            path,
        })
    }

    /// Waits for the copy (submitted with the frame), then writes the PNG.
    pub(crate) fn finish(self, device: &wgpu::Device) -> anyhow::Result<()> {
        let slice = self.buffer.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        let start = Instant::now();
        loop {
            device
                .poll(wgpu::PollType::Poll)
                .map_err(|err| anyhow::anyhow!("polling the device for the snapshot: {err:?}"))?;
            match receiver.recv_timeout(READBACK_POLL_INTERVAL) {
                Ok(Ok(())) => break,
                Ok(Err(err)) => anyhow::bail!("mapping the snapshot buffer failed: {err:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) if start.elapsed() < READBACK_TIMEOUT => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::bail!("timed out mapping the snapshot buffer")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("snapshot mapping callback was dropped")
                }
            }
        }
        let unpadded = self.width as usize * 4;
        let mut texels = Vec::with_capacity(unpadded * self.height as usize);
        {
            let mapped = slice
                .get_mapped_range()
                .map_err(|err| anyhow::anyhow!("reading the mapped snapshot buffer: {err:?}"))?;
            for row in 0..self.height as usize {
                let start = row * self.padded_bytes_per_row as usize;
                texels.extend_from_slice(&mapped[start..start + unpadded]);
            }
        }
        self.buffer.unmap();
        let rgba = texels_to_rgba8(self.format, texels)?;
        write_png_atomically(&self.path, self.width, self.height, &rgba)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_needs_a_non_empty_path_and_defaults_the_title() {
        assert_eq!(RenderSnapshotRequest::from_values(None, None), None);
        assert_eq!(
            RenderSnapshotRequest::from_values(Some("".into()), None),
            None
        );
        let request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), Some(String::new()))
                .expect("request");
        assert_eq!(request.title, DEFAULT_SNAPSHOT_TITLE);
        assert_eq!(request.path, PathBuf::from("/tmp/a.png"));
    }

    #[test]
    fn claim_fires_once_and_only_on_the_sentinel_title() {
        let mut request =
            RenderSnapshotRequest::from_values(Some("/tmp/a.png".into()), Some("done".into()))
                .unwrap();
        assert_eq!(request.claim(None), None);
        assert_eq!(request.claim(Some("zsh")), None);
        assert_eq!(
            request.claim(Some("done")),
            Some(PathBuf::from("/tmp/a.png"))
        );
        assert_eq!(
            request.claim(Some("done")),
            None,
            "a snapshot is taken once"
        );
    }

    #[test]
    fn bgra_texels_are_swizzled_and_rgba_texels_kept() {
        let texels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            texels_to_rgba8(wgpu::TextureFormat::Bgra8UnormSrgb, texels.clone()).unwrap(),
            vec![3, 2, 1, 4, 7, 6, 5, 8]
        );
        assert_eq!(
            texels_to_rgba8(wgpu::TextureFormat::Rgba8Unorm, texels.clone()).unwrap(),
            texels
        );
        assert!(texels_to_rgba8(wgpu::TextureFormat::Rgba16Float, texels).is_err());
    }

    #[test]
    fn png_is_written_atomically_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/frame.png");
        let rgba: Vec<u8> = (0..2 * 3 * 4).map(|i| i as u8).collect();
        write_png_atomically(&path, 2, 3, &rgba).unwrap();
        assert!(!dir.path().join("nested/frame.png.partial").exists());
        let back = image::open(&path).unwrap().into_rgba8();
        assert_eq!(back.dimensions(), (2, 3));
        assert_eq!(back.into_raw(), rgba);
        assert!(
            write_png_atomically(&path, 5, 5, &rgba).is_err(),
            "size mismatch is refused"
        );
    }
}
